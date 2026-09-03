use crate::db_message::{BatchOutcome, DbMessage, OwnedSqlParams};
use crate::progress_bar::ProgressCounter;
use anyhow::Context;
use log::{debug, error};
use std::sync::Arc;
use tokio::sync::mpsc::Sender;
use tokio::sync::oneshot;

pub const DB_BATCH_SIZE: usize = 5000;

pub struct DbWriter {
    db_sender: Sender<DbMessage>,
    query: &'static str,
    batch_size: usize,
    rows: Vec<OwnedSqlParams>,
    progress_counter: Arc<ProgressCounter>,
}

impl DbWriter {
    pub fn new(
        db_sender: Sender<DbMessage>,
        query: &'static str,
        progress_counter: Arc<ProgressCounter>,
    ) -> Self {
        Self::with_batch_size(db_sender, query, progress_counter, DB_BATCH_SIZE)
    }

    pub fn with_batch_size(
        db_sender: Sender<DbMessage>,
        query: &'static str,
        progress_counter: Arc<ProgressCounter>,
        batch_size: usize,
    ) -> Self {
        Self {
            db_sender,
            query,
            batch_size: batch_size.max(1),
            rows: Vec::with_capacity(batch_size.max(1)),
            progress_counter,
        }
    }

    pub async fn push(&mut self, row: OwnedSqlParams) -> anyhow::Result<()> {
        self.rows.push(row);
        self.progress_counter.inc();
        if self.rows.len() >= self.batch_size {
            self.flush().await?;
        }
        Ok(())
    }

    pub async fn flush(&mut self) -> anyhow::Result<()> {
        if self.rows.is_empty() {
            return Ok(());
        }
        let rows = std::mem::replace(&mut self.rows, Vec::with_capacity(self.batch_size));
        let batch_len = rows.len();

        let (response_tx, response_rx) = oneshot::channel();
        self.db_sender
            .send(DbMessage::Execute {
                query: self.query.to_string(),
                rows,
                response: response_tx,
            })
            .await
            .with_context(|| {
                format!(
                    "SQLite consumer is gone, {} rows of `{}` were not written",
                    batch_len, self.query
                )
            })?;
        let outcome: BatchOutcome = response_rx.await.with_context(|| {
            format!(
                "SQLite consumer did not acknowledge {} rows of `{}`",
                batch_len, self.query
            )
        })?;

        anyhow::ensure!(
            outcome.rows_failed == 0,
            "SQLite rejected {} of {} rows of `{}`",
            outcome.rows_failed,
            batch_len,
            self.query
        );
        debug!("Batch of {} rows written for `{}`", batch_len, self.query);
        Ok(())
    }
}

impl Drop for DbWriter {
    fn drop(&mut self) {
        let count = self.rows.len();
        if count == 0 {
            return;
        }
        if self
            .db_sender
            .try_send(DbMessage::RowsLost {
                query: self.query.to_string(),
                count,
            })
            .is_err()
        {
            error!(
                "{} buffered rows of `{}` were dropped unflushed and the loss could not be reported to SQLite",
                count, self.query
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db_message::OwnedSqlValue;
    use crate::sql_params;
    use indicatif::MultiProgress;
    use rusqlite::ToSql;
    use tokio::sync::mpsc;
    use tokio::task::JoinHandle;

    fn counter() -> Arc<ProgressCounter> {
        Arc::new(ProgressCounter::new(&MultiProgress::new(), "test"))
    }

    const QUERY: &str = "INSERT INTO t (v) SELECT ?;";

    fn spawn_collector(
        mut receiver: mpsc::Receiver<DbMessage>,
        failures_per_batch: usize,
    ) -> JoinHandle<Vec<usize>> {
        tokio::spawn(async move {
            let mut batches = Vec::new();
            while let Some(msg) = receiver.recv().await {
                match msg {
                    DbMessage::Execute { rows, response, .. } => {
                        batches.push(rows.len());
                        let rows_failed = failures_per_batch.min(rows.len());
                        let _ = response.send(BatchOutcome { rows_failed });
                    }
                    _ => panic!("unexpected message"),
                }
            }
            batches
        })
    }

    #[tokio::test]
    async fn rows_are_shipped_in_full_batches_and_the_rest_on_flush() {
        let (sender, receiver) = mpsc::channel(4);
        let collector = spawn_collector(receiver, 0);
        let mut writer = DbWriter::with_batch_size(sender, QUERY, counter(), 4);

        for i in 0..5i64 {
            writer.push(sql_params![i]).await.unwrap();
        }
        assert_eq!(writer.rows.len(), 1);

        writer.flush().await.unwrap();
        writer.flush().await.unwrap();

        drop(writer);
        assert_eq!(
            collector.await.unwrap(),
            vec![4, 1],
            "flushing again must not ship an empty batch"
        );
    }

    #[tokio::test]
    async fn a_rejected_row_fails_the_producer_at_its_own_batch() {
        let (sender, receiver) = mpsc::channel(4);
        let collector = spawn_collector(receiver, 1);
        let progress = counter();
        let mut writer = DbWriter::with_batch_size(sender, QUERY, progress.clone(), 3);

        writer.push(sql_params![0i64]).await.unwrap();
        writer.push(sql_params![1i64]).await.unwrap();
        let err = writer
            .push(sql_params![2i64])
            .await
            .expect_err("a batch SQLite rejected a row from must not be reported as written");

        assert!(
            err.to_string().contains("SQLite rejected 1 of 3 rows"),
            "unexpected error: {err}"
        );
        drop(writer);
        assert_eq!(
            collector.await.unwrap(),
            vec![3],
            "no further batch may be shipped after the run is already lost"
        );
        assert_eq!(
            progress.get(),
            3,
            "progress counts rows handed over, not rows SQLite accepted"
        );
    }

    #[tokio::test]
    async fn a_dead_consumer_fails_the_producer_instead_of_dropping_rows() {
        let (sender, receiver) = mpsc::channel(4);
        drop(receiver);
        let mut writer = DbWriter::with_batch_size(sender, QUERY, counter(), 4);

        writer.push(sql_params![1i64]).await.unwrap();
        let err = writer
            .flush()
            .await
            .expect_err("rows that cannot be sent must not be reported as written");
        assert!(
            err.to_string().contains("SQLite consumer is gone"),
            "unexpected error: {err}"
        );
    }

    #[tokio::test]
    async fn a_missing_acknowledgement_fails_the_producer() {
        let (sender, mut receiver) = mpsc::channel(4);
        tokio::spawn(async move {
            let _ = receiver.recv().await;
        });
        let mut writer = DbWriter::with_batch_size(sender, QUERY, counter(), 1);

        let err = writer
            .push(sql_params![1i64])
            .await
            .expect_err("an unacknowledged batch must not be reported as written");
        assert!(
            err.to_string().contains("did not acknowledge"),
            "unexpected error: {err}"
        );
    }
}
