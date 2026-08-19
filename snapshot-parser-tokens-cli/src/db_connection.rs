use crate::db_message::{BatchOutcome, DbMessage, OwnedSqlParams};
use crate::progress_bar::ProgressCounter;
use crate::temp_file::TempFileGuard;
use log::{debug, error, info};
use rusqlite::{params_from_iter, Connection, Params};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::mpsc::Receiver;

pub struct SQLiteExecutor {
    db: Connection,
    db_path: PathBuf,
    db_temp_guard: TempFileGuard,

    tx_bulk: Option<u16>,
    transaction_batch_counter: u16,

    db_execute_counter: Arc<ProgressCounter>,
    /// Rows SQLite refused over the whole run; [`Self::finalize`] refuses to promote a DB
    /// that lost any of them.
    rows_rejected: usize,

    receiver: Receiver<DbMessage>,
    shut_down: bool,
}

impl SQLiteExecutor {
    /// This is a SQLite DB connection wrapper that provides a temporary file for the DB.
    /// This connection strictly requires exclusive locking and has got no journaling set up.
    pub fn new(
        db_path: PathBuf,
        cache_size: Option<i64>,
        mmap_size: Option<u64>,
        tx_bulk: Option<u16>,
        db_execute_counter: Arc<ProgressCounter>,
        receiver: Receiver<DbMessage>,
    ) -> anyhow::Result<Self> {
        // Create temporary DB file, which gets promoted on success.
        let temp_file_name = format!("_{}.tmp", db_path.file_name().unwrap().to_string_lossy());
        let db_temp_path = db_path.with_file_name(&temp_file_name);
        let _ = std::fs::remove_file(&db_temp_path);
        let db_temp_guard = TempFileGuard::new(db_temp_path.clone());
        // Create and configure the DB as file-backed
        let db = Self::connect_db(&db_temp_path, cache_size, mmap_size)
            .map_err(|e| SQLiteExecutor::convert_sqlite_error("new", e))?;

        Ok(Self {
            db,
            db_path,
            db_temp_guard,
            tx_bulk,
            transaction_batch_counter: 0,
            db_execute_counter,
            rows_rejected: 0,
            receiver,
            shut_down: false,
        })
    }

    /// Execute data insertion into the DB within transaction processing.
    pub async fn execute<P: Params>(&mut self, sql: &str, params: P) -> anyhow::Result<usize> {
        if self.tx_bulk.is_some() && self.transaction_batch_counter == 0 {
            // we explicitly start transaction bulk here, otherwise every insert will be a separate transaction that fsync to disk
            self.db
                .execute_batch("BEGIN;")
                .map_err(|e| SQLiteExecutor::convert_sqlite_error("execute", e))?;
            // it should not start a new transaction when multiple `begin_transaction` called in row
            self.transaction_batch_counter = 1;
        }

        // Fast operation due to SQLite's internal cache
        let mut stmt = self
            .db
            .prepare(sql)
            .map_err(|e| SQLiteExecutor::convert_sqlite_error("execute:prepare", e))?;

        self.transaction_batch_counter = self.transaction_batch_counter.saturating_add(1);
        let result = stmt
            .execute(params)
            .map_err(|e| SQLiteExecutor::convert_sqlite_error("execute:statement", e))?;
        self.db_execute_counter.inc();
        drop(stmt);

        if let Some(bulk_size) = self.tx_bulk {
            if self.transaction_batch_counter.is_multiple_of(bulk_size)
                || self.transaction_batch_counter == u16::MAX
            {
                self.commit_db("execute");
            }
        }
        Ok(result)
    }

    /// Execute one batch of rows of the same statement.
    ///
    /// Batching is about the channel, not about SQL: the rows are still executed one by
    /// one, so a row SQLite rejects is logged and skipped exactly as it was when every
    /// row travelled on its own, and its neighbours in the batch still land. The
    /// transaction bulk therefore keeps counting rows, not batches.
    ///
    /// A rejected row never aborts the run mid-write - the other producers are still
    /// writing, and killing them here would leave a half-written DB behind. The rejects
    /// are tallied instead, and [`Self::finalize`] decides once, at the end, whether the
    /// DB is worth promoting.
    pub async fn execute_rows(&mut self, sql: &str, rows: Vec<OwnedSqlParams>) -> BatchOutcome {
        let mut outcome = BatchOutcome::default();
        for params in rows {
            // the error is already logged by convert_sqlite_error
            match self.execute(sql, params_from_iter(params.iter())).await {
                Ok(_) => outcome.rows_written += 1,
                Err(_) => outcome.rows_failed += 1,
            }
        }
        self.rows_rejected = self.rows_rejected.saturating_add(outcome.rows_failed);
        outcome
    }

    /// Usable for special cases when quiting transaction is required.
    /// Use only for really special cases that are un-usual like creating tables and similar.
    pub async fn execute_special<P: Params>(
        &mut self,
        sql: &str,
        params: P,
    ) -> anyhow::Result<usize> {
        // closing any open transaction
        if self.tx_bulk.is_some() && self.transaction_batch_counter > 0 {
            self.commit_db("execute_special");
        }

        debug!("Executing special out-of-transaction SQL: {}", sql);
        let result = self
            .db
            .execute(sql, params)
            .map_err(|e| SQLiteExecutor::convert_sqlite_error("execute_special:execute", e))?;

        Ok(result)
    }

    fn connect_db(
        path: &Path,
        cache_size_mib: Option<i64>,
        mmap_size_mib: Option<u64>,
    ) -> rusqlite::Result<Connection> {
        let db = Connection::open(path)?;
        db.pragma_update(None, "synchronous", false)?;
        db.pragma_update(None, "journal_mode", "off")?;
        db.pragma_update(None, "locking_mode", "exclusive")?;
        db.pragma_update(None, "temp_store", "memory")?;
        if let Some(size_mib) = cache_size_mib {
            db.pragma_update(None, "cache_size", cache_size_pragma(size_mib))?;
        }
        if let Some(size_mib) = mmap_size_mib {
            db.pragma_update(None, "mmap_size", mmap_size_pragma(size_mib))?;
        }
        Ok(db)
    }

    pub async fn start(mut self) {
        if self.shut_down {
            error!("SQLiteExecutor already shut down");
            return;
        }

        info!("SQLiteExecutor receiver started to listen for SQL insertion messages");
        while let Some(msg) = self.receiver.recv().await {
            match msg {
                DbMessage::Execute {
                    query,
                    rows,
                    response,
                } => {
                    let outcome = self.execute_rows(&query, rows).await;
                    // one acknowledgement for the whole batch
                    let _ = response.send(outcome);
                }
                DbMessage::ExecuteSpecial {
                    query,
                    params,
                    response,
                } => {
                    let result = self
                        .execute_special(&query, params_from_iter(params.iter()))
                        .await;
                    let _ = response.send(result);
                }
                DbMessage::Shutdown { response } => {
                    let result = self.finalize().await;
                    if result.is_ok() {
                        self.shut_down = true;
                    }
                    let _ = response.send(result);
                }
            }
        }
    }

    pub async fn finalize(&mut self) -> anyhow::Result<()> {
        // first, commit transactions if there is some started
        if self.tx_bulk.is_some() && self.transaction_batch_counter > 0 {
            self.commit_db("finalize");
        }

        // A single rejected row fails the run, rather than a share of them: none of the
        // rows the processors push can be rejected on its own merits any more. Accounts
        // that do not unpack are filtered out before the push, a voting power that does
        // not add up is skipped by its processor, every NOT NULL column is filled by
        // construction, and every statement is an INSERT OR REPLACE, so a repeated pubkey
        // replaces instead of conflicting. What is left to reject a row is the storage
        // failing underneath the run - a full disk, an I/O error, corruption - which hits
        // an arbitrary number of rows and leaves a DB that is silently short. Promoting
        // that is worse than failing: the manager cannot tell it from a complete one.
        if self.rows_rejected > 0 {
            anyhow::bail!(
                "SQLite rejected {} rows during the run, \
                 refusing to promote an incomplete DB to {:?}",
                self.rows_rejected,
                self.db_path
            );
        }

        // second, promote the DB file as finished
        let db_path = self.db_path.clone();
        self.db_temp_guard.promote(db_path)?;
        info!(
            "SQLite DB file promoted to: {:?} and finalized",
            self.db_path
        );
        Ok(())
    }

    fn commit_db(&mut self, method_name: &str) {
        self.db
            .execute_batch("COMMIT;")
            .map_err(|e| {
                SQLiteExecutor::convert_sqlite_error(format!("{}:commit", method_name).as_str(), e)
            })
            .unwrap();
        self.transaction_batch_counter = 0;
    }

    fn convert_sqlite_error(method: &str, err: rusqlite::Error) -> anyhow::Error {
        let msg = format!("SQLite error at {}: {}", method, err);
        error!("Sqlite error: {}", msg);
        anyhow::Error::msg(msg)
    }
}

const BYTES_PER_MIB: u64 = 1024 * 1024;

/// `PRAGMA cache_size` counts pages when positive and *kibibytes* when negative, so a
/// size in MiB is passed as -(MiB * 1024). Saturating, because the whole workspace is
/// built with overflow checks on and this comes straight from the command line.
fn cache_size_pragma(size_mib: i64) -> i64 {
    size_mib.saturating_mul(1024).saturating_neg()
}

/// `PRAGMA mmap_size` is in *bytes*. This used to pass MiB * 1024 (kibibytes, 1024x too
/// small) computed in u16, which overflowed for any size of 64 MiB or more.
fn mmap_size_pragma(size_mib: u64) -> i64 {
    i64::try_from(size_mib.saturating_mul(BYTES_PER_MIB)).unwrap_or(i64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db_message::{OwnedSqlParams, OwnedSqlValue};
    use crate::sql_params;
    use indicatif::MultiProgress;
    use rusqlite::ToSql;
    use std::sync::atomic::{AtomicU64, Ordering};
    use tokio::sync::{mpsc, oneshot};

    const CREATE_TABLE: &str = "CREATE TABLE t (k TEXT NOT NULL PRIMARY KEY, v INTEGER NOT NULL);";
    const INSERT_ROW: &str = "INSERT OR REPLACE INTO t (k, v) SELECT ?, ?;";

    // A directory of its own per test, removed when the test ends.
    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let path = std::env::temp_dir().join(format!(
                "snapshot-parser-tokens-cli-test-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            let _ = std::fs::remove_dir_all(&path);
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }

        fn db_path(&self) -> PathBuf {
            self.0.join("snapshot.db")
        }

        // the name SQLiteExecutor::new derives for the file it writes before promoting
        fn temp_db_path(&self) -> PathBuf {
            self.0.join("_snapshot.db.tmp")
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn row(key: &str, value: Option<i64>) -> OwnedSqlParams {
        sql_params![key, value]
    }

    fn counter() -> Arc<ProgressCounter> {
        Arc::new(ProgressCounter::new(&MultiProgress::new(), "db_execute"))
    }

    // Runs one executor over the given messages and shuts it down, returning what each
    // batch reported and what the shutdown (finalize) answered. The executor is dropped
    // before returning, so its temporary file guard has already had its say.
    async fn run_executor(
        db_path: PathBuf,
        tx_bulk: Option<u16>,
        counter: Arc<ProgressCounter>,
        batches: Vec<Vec<OwnedSqlParams>>,
    ) -> (Vec<BatchOutcome>, anyhow::Result<()>) {
        let (sender, receiver) = mpsc::channel(8);
        let executor =
            SQLiteExecutor::new(db_path, None, None, tx_bulk, counter, receiver).unwrap();
        let task = tokio::spawn(async move { executor.start().await });

        let (created_tx, created_rx) = oneshot::channel();
        sender
            .send(DbMessage::ExecuteSpecial {
                query: CREATE_TABLE.to_string(),
                params: vec![],
                response: created_tx,
            })
            .await
            .unwrap();
        created_rx.await.unwrap().unwrap();

        let mut outcomes = Vec::new();
        for rows in batches {
            let (response_tx, response_rx) = oneshot::channel();
            sender
                .send(DbMessage::Execute {
                    query: INSERT_ROW.to_string(),
                    rows,
                    response: response_tx,
                })
                .await
                .unwrap();
            outcomes.push(response_rx.await.unwrap());
        }

        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        sender
            .send(DbMessage::Shutdown {
                response: shutdown_tx,
            })
            .await
            .unwrap();
        let finalized = shutdown_rx.await.unwrap();
        drop(sender);
        task.await.unwrap();
        (outcomes, finalized)
    }

    fn stored_rows(db_path: &Path) -> Vec<(String, i64)> {
        let db = Connection::open(db_path).unwrap();
        let mut stmt = db.prepare("SELECT k, v FROM t ORDER BY k;").unwrap();
        let rows = stmt
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
            .unwrap()
            .collect::<rusqlite::Result<Vec<(String, i64)>>>()
            .unwrap();
        rows
    }

    // The row-level tolerance of the per-row channel has to survive batching: the
    // consumer executes the rows of a batch one by one, so a rejected row takes only
    // itself down *while the run is going on*. The verdict is passed once, at finalize.
    #[tokio::test]
    async fn a_row_sqlite_rejects_does_not_discard_its_batch() {
        let dir = TempDir::new();
        let progress = counter();
        let (outcomes, finalized) = run_executor(
            dir.db_path(),
            None,
            progress.clone(),
            // the middle row violates NOT NULL
            vec![vec![row("a", Some(1)), row("bad", None), row("c", Some(3))]],
        )
        .await;

        assert_eq!(
            outcomes,
            vec![BatchOutcome {
                rows_written: 2,
                rows_failed: 1
            }]
        );
        // only the rows that made it into the DB are counted as executed
        assert_eq!(progress.get(), 2);
        // the neighbours landed, so the run went on - but the DB is short of a row
        finalized.expect_err("a run that lost a row must not be finalized");
    }

    // A DB that is missing rows must not reach the manager: it is indistinguishable from
    // a complete one, so the run fails and takes its temporary file with it.
    #[tokio::test]
    async fn a_rejected_row_refuses_to_promote_the_db() {
        let dir = TempDir::new();
        let (_, finalized) = run_executor(
            dir.db_path(),
            Some(2),
            counter(),
            vec![
                vec![row("a", Some(1)), row("bad", None)],
                vec![row("worse", None), row("c", Some(3))],
            ],
        )
        .await;

        let err = finalized.expect_err("a run that lost rows must not be promoted");
        assert!(
            err.to_string().contains("rejected 2 rows"),
            "the error must name how many rows were lost: {err}"
        );
        assert!(
            !dir.db_path().exists(),
            "the output path must be left untouched"
        );
        assert!(
            !dir.temp_db_path().exists(),
            "the unpromoted temporary DB must be cleaned up"
        );
    }

    // The other side of the same rule: a run that lost nothing lands on the output path
    // and leaves no temporary file behind.
    #[tokio::test]
    async fn a_run_without_rejects_promotes_the_db() {
        let dir = TempDir::new();
        let (_, finalized) = run_executor(
            dir.db_path(),
            Some(2),
            counter(),
            vec![vec![row("a", Some(1)), row("b", Some(2))]],
        )
        .await;

        finalized.expect("a run without a single rejected row must be promoted");
        assert_eq!(
            stored_rows(&dir.db_path()),
            vec![("a".to_string(), 1), ("b".to_string(), 2)]
        );
        assert!(!dir.temp_db_path().exists());
    }

    // One message in, one acknowledgement out, whatever the batch holds.
    #[tokio::test]
    async fn every_batch_is_acknowledged_exactly_once() {
        let dir = TempDir::new();
        let (outcomes, finalized) = run_executor(
            dir.db_path(),
            None,
            counter(),
            vec![
                vec![row("a", Some(1)), row("b", Some(2))],
                // a partial last batch, as a producer's final flush delivers it
                vec![row("c", Some(3))],
            ],
        )
        .await;

        finalized.unwrap();
        assert_eq!(
            outcomes,
            vec![
                BatchOutcome {
                    rows_written: 2,
                    rows_failed: 0
                },
                BatchOutcome {
                    rows_written: 1,
                    rows_failed: 0
                }
            ]
        );
        assert_eq!(
            stored_rows(&dir.db_path()),
            vec![
                ("a".to_string(), 1),
                ("b".to_string(), 2),
                ("c".to_string(), 3)
            ]
        );
    }

    // The transaction bulk counts rows, so a batch bigger than the bulk is committed in
    // pieces and nothing is left in an open transaction when the DB is promoted.
    #[tokio::test]
    async fn a_batch_bigger_than_the_transaction_bulk_is_written_whole() {
        let dir = TempDir::new();
        let rows: Vec<OwnedSqlParams> = (0..5)
            .map(|i| row(&format!("k{i}"), Some(i as i64)))
            .collect();

        let (outcomes, finalized) =
            run_executor(dir.db_path(), Some(2), counter(), vec![rows]).await;

        finalized.unwrap();
        assert_eq!(
            outcomes,
            vec![BatchOutcome {
                rows_written: 5,
                rows_failed: 0
            }]
        );
        assert_eq!(stored_rows(&dir.db_path()).len(), 5);
    }

    // SQLite wants bytes; the CLI takes MiB. The old code passed KiB computed in u16, so
    // --sqlite-mmap-size 64 already overflowed and anything smaller was 1024x too small.
    #[test]
    fn mmap_size_is_passed_in_bytes_and_does_not_overflow() {
        assert_eq!(mmap_size_pragma(0), 0);
        assert_eq!(mmap_size_pragma(1), 1024 * 1024);
        assert_eq!(mmap_size_pragma(64), 67_108_864);
        assert_eq!(mmap_size_pragma(4096), 4_294_967_296);
        // a size that cannot be expressed in bytes is clamped, never wrapped
        assert_eq!(mmap_size_pragma(u64::MAX), i64::MAX);
    }

    // cache_size in MiB is negated kibibytes, which was already right; only the overflow
    // at the extremes is new.
    #[test]
    fn cache_size_is_passed_as_negative_kibibytes() {
        assert_eq!(cache_size_pragma(0), 0);
        assert_eq!(cache_size_pragma(1), -1024);
        assert_eq!(cache_size_pragma(64), -65_536);
        assert_eq!(cache_size_pragma(4096), -4_194_304);
        assert_eq!(cache_size_pragma(i64::MAX), i64::MIN + 1);
    }

    // Both pragmas have to be accepted by SQLite itself, not just computed. 64 MiB is the
    // smallest size the u16 arithmetic used to overflow on.
    #[test]
    fn sqlite_accepts_the_configured_pragmas() {
        let dir = TempDir::new();
        let mmap_size = |size_mib| {
            let db =
                SQLiteExecutor::connect_db(&dir.db_path(), Some(4096), Some(size_mib)).unwrap();
            db.pragma_query_value(None, "mmap_size", |row| row.get::<_, i64>(0))
                .unwrap()
        };

        assert_eq!(mmap_size(64), 67_108_864);
        // bigger sizes are capped by SQLITE_MAX_MMAP_SIZE, but must still be memory mapped
        assert!(mmap_size(4096) >= 67_108_864);
    }
}
