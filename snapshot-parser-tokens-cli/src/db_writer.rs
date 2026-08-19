use crate::db_message::{BatchOutcome, DbMessage, OwnedSqlParams};
use crate::progress_bar::ProgressCounter;
use anyhow::Context;
use log::{debug, error};
use std::sync::Arc;
use tokio::sync::mpsc::Sender;
use tokio::sync::oneshot;

/// Rows buffered by a producer before one message is handed to the SQLite consumer.
///
/// Every row used to be its own channel message with its own oneshot acknowledgement,
/// which lock-stepped all producers behind the single writer: the round-trip, not the
/// SQL, dominated the write path (batching measured 2.31x on it). 5000 rows amortise the
/// round-trip away while keeping the in-flight memory small: a producer never has more
/// than one batch outstanding, so at most one batch per producer is queued.
pub const DB_BATCH_SIZE: usize = 5000;

/// Accumulates rows of one statement and ships them to the SQLite consumer in batches.
///
/// Error semantics match the per-row path this replaces: a row SQLite rejects is logged
/// and skipped (by the consumer, which executes rows individually), while losing the
/// channel or the acknowledgement is fatal for the producer - the consumer is gone, so
/// every later row would be dropped silently.
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

    /// Buffers one row, shipping the batch once it is full.
    pub async fn push(&mut self, row: OwnedSqlParams) -> anyhow::Result<()> {
        self.rows.push(row);
        // the counter follows the row into the buffer, as it did when every row was sent
        // on its own: it counts rows handed over, not rows SQLite accepted
        self.progress_counter.inc();
        if self.rows.len() >= self.batch_size {
            self.flush().await?;
        }
        Ok(())
    }

    /// Ships whatever is buffered, including a partial batch. Must be called once the
    /// producer is done, otherwise its last rows never reach the DB.
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
        // one acknowledgement for the whole batch
        let outcome: BatchOutcome = response_rx.await.with_context(|| {
            format!(
                "SQLite consumer did not acknowledge {} rows of `{}`",
                batch_len, self.query
            )
        })?;

        if outcome.rows_failed > 0 {
            // the consumer logged each rejected row; this is the per-batch tally
            error!(
                "SQLite rejected {} of {} rows of `{}`",
                outcome.rows_failed, batch_len, self.query
            );
        }
        debug!(
            "Batch of {} rows written for `{}` ({} failed)",
            batch_len, self.query, outcome.rows_failed
        );
        Ok(())
    }
}

impl Drop for DbWriter {
    fn drop(&mut self) {
        // dropping cannot flush (that needs to await), so a producer that forgot to flush
        // would lose its last rows without a trace
        if !self.rows.is_empty() {
            error!(
                "{} buffered rows of `{}` were dropped unflushed",
                self.rows.len(),
                self.query
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

    // Drains the channel like the SQLite consumer does, acknowledging every batch and
    // recording how many rows each one carried.
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
                        let _ = response.send(BatchOutcome {
                            rows_written: rows.len() - rows_failed,
                            rows_failed,
                        });
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
        // the fifth row is still buffered: only the full batch has been shipped
        assert_eq!(writer.rows.len(), 1);

        writer.flush().await.unwrap();
        // flushing again must not ship an empty batch
        writer.flush().await.unwrap();

        drop(writer);
        assert_eq!(collector.await.unwrap(), vec![4, 1]);
    }

    #[tokio::test]
    async fn a_batch_is_acknowledged_once_and_rejected_rows_are_only_counted() {
        let (sender, receiver) = mpsc::channel(4);
        let collector = spawn_collector(receiver, 1);
        let progress = counter();
        let mut writer = DbWriter::with_batch_size(sender, QUERY, progress.clone(), 3);

        for i in 0..6i64 {
            writer.push(sql_params![i]).await.unwrap();
        }
        writer.flush().await.unwrap();

        // two batches, one acknowledgement each, and neither rejected row stopped the producer
        drop(writer);
        assert_eq!(collector.await.unwrap(), vec![3, 3]);
        assert_eq!(progress.get(), 6);
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
            // takes the batch, then drops the response sender without answering
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
