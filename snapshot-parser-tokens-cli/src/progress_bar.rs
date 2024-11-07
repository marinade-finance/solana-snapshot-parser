use crate::stats::ProcessorCallback;
use async_trait::async_trait;
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

pub fn create_spinner_progress_bar(name: String) -> ProgressBar {
    let spinner_style = ProgressStyle::with_template(
        "{prefix:>20.bold.dim} {spinner} rate={per_sec:>13} total={human_pos:>11}",
    )
    .unwrap();
    ProgressBar::new_spinner()
        .with_style(spinner_style)
        .with_prefix(name)
}

pub fn create_finalization_progress_bar(total_number_of_tables: u64) -> ProgressBar {
    let progress_bar_style =
        ProgressStyle::with_template("{prefix:>20.bold.dim} [{bar:30}] {pos:>1}/{len:>1}")
            .unwrap()
            .progress_chars("#>-");
    ProgressBar::new(total_number_of_tables)
        .with_style(progress_bar_style)
        .with_prefix("finalization")
}

pub struct ProgressCounter {
    name: String,
    progress_bar: Mutex<ProgressBar>,
    counter: AtomicU64,
}

impl ProgressCounter {
    pub fn new(multi_progress: &MultiProgress, name: &str) -> ProgressCounter {
        let name_string = name.to_string();
        let progress_bar = create_spinner_progress_bar(name_string.clone());
        let multi_progress_bar = multi_progress.add(progress_bar);
        Self {
            name: name_string,
            progress_bar: Mutex::new(multi_progress_bar),
            counter: AtomicU64::new(0),
        }
    }

    pub fn get(&self) -> u64 {
        self.counter.load(Ordering::Relaxed)
    }

    pub fn inc(&self) {
        let count = self.counter.fetch_add(1, Ordering::Relaxed);
        if count % 1024 == 0 {
            self.progress_bar.lock().unwrap().set_position(count)
        }
    }
}

impl Into<u64> for ProgressCounter {
    fn into(self) -> u64 {
        self.get()
    }
}

#[async_trait]
impl ProcessorCallback for ProgressCounter {
    async fn get_count(&self) -> (String, u64) {
        (self.name.clone(), self.get())
    }
}

impl Drop for ProgressCounter {
    fn drop(&mut self) {
        let progress_bar = self.progress_bar.lock().unwrap();
        progress_bar.set_position(self.get());
        progress_bar.finish();
    }
}
