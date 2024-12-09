use log::{debug, info};
use std::future::Future;
use tokio::task::JoinHandle;

pub trait Processor {
    fn name() -> &'static str;
    fn process(&mut self) -> impl Future<Output = anyhow::Result<()>> + Send;
}

pub async fn spawn_processor_task<P: Processor + Send + 'static>(
    mut processor: P,
) -> anyhow::Result<JoinHandle<anyhow::Result<()>>> {
    Ok(tokio::spawn(async move {
        info!("{} processor task started...", P::name());
        processor.process().await?;
        debug!("{} processor task finished", P::name());
        Ok(())
    }))
}
