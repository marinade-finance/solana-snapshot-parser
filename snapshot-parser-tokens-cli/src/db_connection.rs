use crate::db_message::DbMessage;
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

    receiver: Receiver<DbMessage>,
    shut_down: bool,
}

impl SQLiteExecutor {
    /// This is a SQLite DB connection wrapper that provides a temporary file for the DB.
    /// This connection strictly requires exclusive locking and has got no journaling set up.
    pub fn new(
        db_path: PathBuf,
        cache_size: Option<i64>,
        mmap_size: Option<u16>,
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
        let db = Self::connect_db(&db_temp_path, cache_size, mmap_size)?;

        Ok(Self {
            db,
            db_path,
            db_temp_guard,
            tx_bulk,
            transaction_batch_counter: 0,
            db_execute_counter,
            receiver,
            shut_down: false,
        })
    }

    /// Execute data insertion into the DB within transaction processing.
    pub async fn execute<P: Params>(&mut self, sql: &str, params: P) -> anyhow::Result<usize> {
        if self.tx_bulk.is_some() && self.transaction_batch_counter == 0 {
            // we explicitly start transaction bulk here, otherwise every insert will be a separate transaction that fsync to disk
            self.db.execute_batch("BEGIN;")?;
            // it should not start a new transaction when multiple `begin_transaction` called in row
            self.transaction_batch_counter = 1;
        }

        // Fast operation due to SQLite's internal cache
        let mut stmt = self.db.prepare(sql)?;

        self.transaction_batch_counter = self.transaction_batch_counter.saturating_add(1);
        let result = stmt.execute(params).map_err(Into::into);
        self.db_execute_counter.inc();

        if let Some(bulk_size) = self.tx_bulk {
            if self.transaction_batch_counter % bulk_size == 0
                || self.transaction_batch_counter == u16::MAX
            {
                self.db.execute_batch("COMMIT;")?;
                self.transaction_batch_counter = 0;
            }
        }
        result
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
            self.db.execute_batch("COMMIT;")?;
        }

        debug!("Executing special out-of-transaction SQL: {}", sql);
        let result = self.db.execute(sql, params).map_err(Into::into);

        // let's start a new transaction when we committed the previous one
        if let Some(bulk_size) = self.tx_bulk {
            if self.transaction_batch_counter % bulk_size == 0 {
                self.db.execute_batch("BEGIN;")?;
                self.transaction_batch_counter = 1;
            }
        }

        result
    }

    fn connect_db(
        path: &Path,
        cache_size_mb: Option<i64>,
        mmap_size_mb: Option<u16>,
    ) -> anyhow::Result<Connection> {
        let db = Connection::open(&path)?;
        db.pragma_update(None, "synchronous", false)?;
        db.pragma_update(None, "journal_mode", "off")?;
        db.pragma_update(None, "locking_mode", "exclusive")?;
        db.pragma_update(None, "temp_store", "memory")?;
        if let Some(size_mib) = cache_size_mb {
            let size = size_mib * 1024;
            db.pragma_update(None, "cache_size", -size)?;
        }
        if let Some(size_mib) = mmap_size_mb {
            let size_kb = size_mib * 1024;
            db.pragma_update(None, "mmap_size", size_kb)?;
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
                    params,
                    response,
                } => {
                    let result = self.execute(&query, params_from_iter(params.iter())).await;
                    let _ = response.send(result);
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
            self.db.execute_batch("COMMIT;")?;
        }

        // second, promote the DB file as finished
        let db_path = self.db_path.clone();
        self.db_temp_guard.promote(db_path)?;
        info!(
            "SQLite DB file promoted to: {:?} and finalized",
            &self.db_path
        );
        Ok(())
    }
}
