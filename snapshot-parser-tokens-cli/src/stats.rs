use async_trait::async_trait;
use log::info;
use std::sync::Arc;
use tokio::sync::Mutex;
use tokio::time::Instant;

#[async_trait]
pub trait ProcessorCallback: Send + Sync {
    async fn get_count(&self) -> (String, u64);
}

pub struct Stats {
    inserts_time: Instant,
    callbacks: Arc<Mutex<Vec<Arc<dyn ProcessorCallback>>>>,
}

impl Default for Stats {
    fn default() -> Self {
        Self::new()
    }
}

impl Stats {
    pub fn new() -> Self {
        Self {
            inserts_time: Instant::now(),
            callbacks: Arc::new(Mutex::new(Vec::new())),
        }
    }

    pub async fn add_callback(&self, callback: Arc<dyn ProcessorCallback>) {
        let mut self_callbacks = self.callbacks.lock().await;
        self_callbacks.push(callback);
    }

    pub async fn add_callbacks(&self, callbacks: &[Arc<dyn ProcessorCallback>]) {
        let mut self_callbacks = self.callbacks.lock().await;
        self_callbacks.extend(callbacks.iter().cloned());
    }

    fn info(msg: &str, value: u64) {
        info!("Dumped {} {} accounts", msg, value);
    }

    pub async fn print_info(&self) {
        let insert_duration = Instant::now() - self.inserts_time;
        info!("Done! (sqlite processing in {:?})", insert_duration);

        let callbacks = self.callbacks.lock().await;
        for callback in callbacks.iter() {
            let (name, value) = callback.get_count().await;
            Stats::info(&name, value);
        }
    }
}
