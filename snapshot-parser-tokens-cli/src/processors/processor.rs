use log::{debug, error, info};
use std::future::Future;
use tokio::task::JoinHandle;

pub trait Processor {
    fn name() -> &'static str;
    fn process(&mut self) -> impl Future<Output = anyhow::Result<()>> + Send;
}

/// A spawned processor, kept together with its name so a failure can say which one it was.
pub struct ProcessorTask {
    name: &'static str,
    handle: JoinHandle<anyhow::Result<()>>,
}

impl ProcessorTask {
    pub fn new(name: &'static str, handle: JoinHandle<anyhow::Result<()>>) -> Self {
        Self { name, handle }
    }
}

pub async fn spawn_processor_task<P: Processor + Send + 'static>(
    mut processor: P,
) -> anyhow::Result<ProcessorTask> {
    let handle = tokio::spawn(async move {
        info!("{} processor task started...", P::name());
        processor.process().await?;
        debug!("{} processor task finished", P::name());
        Ok(())
    });
    Ok(ProcessorTask::new(P::name(), handle))
}

/// Waits for every processor and reports the first failure.
///
/// Every task is awaited before returning, so no processor is killed mid-write, and a
/// task that failed or panicked must fail the run: it leaves a truncated DB behind, and
/// a truncated DB that gets promoted and ingested looks exactly like a successful one.
pub async fn join_processor_tasks(
    tasks: impl IntoIterator<Item = ProcessorTask>,
) -> anyhow::Result<()> {
    let mut failure = None;
    for ProcessorTask { name, handle } in tasks {
        let outcome = match handle.await {
            Ok(Ok(())) => {
                debug!("{name} processor completed successfully.");
                continue;
            }
            Ok(Err(err)) => format!("Error in {name} processor: {err:?}"),
            Err(err) => format!("{name} processor panicked: {err:?}"),
        };
        error!("{outcome}");
        failure = failure.or(Some(outcome));
    }

    if let Some(failure) = failure {
        anyhow::bail!(failure);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    fn task(
        name: &'static str,
        body: impl Future<Output = anyhow::Result<()>> + Send + 'static,
    ) -> ProcessorTask {
        ProcessorTask::new(name, tokio::spawn(body))
    }

    #[tokio::test]
    async fn all_tasks_succeeding_is_a_successful_run() {
        let finished = Arc::new(AtomicUsize::new(0));
        let tasks: Vec<ProcessorTask> = (0..3)
            .map(|_| {
                let finished = finished.clone();
                task("ok", async move {
                    finished.fetch_add(1, Ordering::Relaxed);
                    Ok(())
                })
            })
            .collect();

        join_processor_tasks(tasks).await.unwrap();
        assert_eq!(finished.load(Ordering::Relaxed), 3);
    }

    // A processor that gave up wrote only part of its accounts; the run must not look
    // like it produced a complete DB.
    #[tokio::test]
    async fn a_failing_processor_fails_the_run() {
        let err = join_processor_tasks([
            task("Token", async { Ok(()) }),
            task("VeMnde", async { anyhow::bail!("unpack failed") }),
            task("Native Stake", async { Ok(()) }),
        ])
        .await
        .expect_err("a processor that failed must not be reported as a finished run");

        assert!(
            err.to_string().contains("VeMnde"),
            "unexpected error: {err}"
        );
        assert!(
            err.to_string().contains("unpack failed"),
            "unexpected error: {err}"
        );
    }

    #[tokio::test]
    async fn a_panicking_processor_fails_the_run() {
        let err = join_processor_tasks([
            task("Token", async { Ok(()) }),
            task("Mint", async { panic!("processor blew up") }),
        ])
        .await
        .expect_err("a processor that panicked must not be reported as a finished run");

        assert!(
            err.to_string().contains("Mint processor panicked"),
            "unexpected error: {err}"
        );
    }

    // Returning at the first failure would leave the other processors writing into a DB
    // that is being finalized, so every task is awaited first.
    #[tokio::test]
    async fn every_task_is_awaited_even_after_a_failure() {
        let finished = Arc::new(AtomicUsize::new(0));
        let late = {
            let finished = finished.clone();
            task("Late", async move {
                tokio::task::yield_now().await;
                finished.fetch_add(1, Ordering::Relaxed);
                Ok(())
            })
        };

        let err = join_processor_tasks([task("Early", async { anyhow::bail!("first") }), late])
            .await
            .expect_err("the run must fail");

        assert!(err.to_string().contains("Early"), "unexpected error: {err}");
        assert_eq!(finished.load(Ordering::Relaxed), 1);
    }
}
