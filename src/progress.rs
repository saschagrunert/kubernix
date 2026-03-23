use console::style;
use indicatif::{ProgressBar, ProgressStyle};
use log::LevelFilter;
use parking_lot::RwLock;
use std::{
    sync::{Arc, OnceLock, Weak},
    time::Duration,
};

pub struct Progress {
    inner: Option<Arc<ProgressBar>>,
}

static PROGRESS_BAR: OnceLock<RwLock<Option<Weak<ProgressBar>>>> = OnceLock::new();

fn progress_bar() -> &'static RwLock<Option<Weak<ProgressBar>>> {
    PROGRESS_BAR.get_or_init(|| RwLock::new(None))
}

impl Progress {
    // Create a new global progress bar
    pub fn new(items: u64, level: LevelFilter) -> Progress {
        if level < LevelFilter::Info {
            return Progress { inner: None };
        }

        // Create the progress bar
        let p = Arc::new(ProgressBar::new(items));
        p.set_style(
            ProgressStyle::default_bar()
                .template(&format!(
                    "{}{}{} {}",
                    style("[").white().dim(),
                    "{spinner:.green} {elapsed:>3}",
                    style("]").white().dim(),
                    "{bar:25.green/blue} {pos:>2}/{len} {msg}",
                ))
                .expect("invalid progress bar template"),
        );
        p.enable_steady_tick(Duration::from_millis(100));

        // Set the global instance
        *progress_bar().write() = Some(Arc::downgrade(&p));

        Progress { inner: Some(p) }
    }

    // Get the progress bar
    pub fn get() -> Option<Arc<ProgressBar>> {
        progress_bar().read().as_ref()?.upgrade()
    }

    // Reset and consume the progress bar
    pub fn reset(self) {
        if let Some(p) = self.inner {
            p.finish()
        }
        *progress_bar().write() = None;
    }
}

#[cfg(test)]
pub mod tests {
    use super::*;

    #[test]
    fn progress_success() {
        let p = Progress::new(10, LevelFilter::Info);
        assert!(Progress::get().is_some());
        p.reset();
        assert!(Progress::get().is_none());
    }
}
