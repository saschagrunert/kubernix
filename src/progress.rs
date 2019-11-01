use console::style;
use indicatif::{ProgressBar, ProgressStyle};
use lazy_static::lazy_static;
use log::LevelFilter;
use parking_lot::RwLock;
use std::sync::{Arc, Weak};

pub struct Progress {
    inner: Option<Arc<ProgressBar>>,
}

lazy_static! {
    static ref PROGRESS_BAR: RwLock<Option<Weak<ProgressBar>>> = RwLock::new(None);
}

impl Progress {
    // Create a new global progress bar
    pub fn new(items: u64, level: LevelFilter) -> Progress {
        if level < LevelFilter::Info {
            return Progress { inner: None };
        }

        // Create the progress bar
        let p = Arc::new(ProgressBar::new(items));
        p.set_style(ProgressStyle::default_bar().template(&format!(
            "{}{}{} {}",
            style("[").white().dim(),
            "{spinner:.green} {elapsed:>3}",
            style("]").white().dim(),
            "{bar:25.green/blue} {pos:>2}/{len} {msg}",
        )));
        p.enable_steady_tick(100);

        // Set the global instance
        *PROGRESS_BAR.write() = Some(Arc::downgrade(&p));

        Progress { inner: Some(p) }
    }

    // Get the progress bar
    pub fn get() -> Option<Arc<ProgressBar>> {
        PROGRESS_BAR.read().as_ref()?.upgrade()
    }

    // Reset and consume the progress bar
    pub fn reset(self) {
        if let Some(p) = self.inner {
            p.finish()
        }
        *PROGRESS_BAR.write() = None;
    }
}
