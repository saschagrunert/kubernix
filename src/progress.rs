//! Thread-safe global progress bar for cluster bootstrap stages.
//!
//! A single [`Progress`] instance owns the underlying [`ProgressBar`] via
//! `Arc`. Other parts of the codebase (e.g. the logger) can obtain a
//! weak reference through [`Progress::get`] to update or print alongside
//! the bar without taking ownership.

use console::style;
use indicatif::{ProgressBar, ProgressStyle};
use log::LevelFilter;
use std::{
    sync::{Arc, Mutex, OnceLock, Weak},
    time::Duration,
};

#[must_use]
pub struct Progress {
    inner: Option<Arc<ProgressBar>>,
}

static PROGRESS_BAR: OnceLock<Mutex<Option<Weak<ProgressBar>>>> = OnceLock::new();

fn progress_bar() -> &'static Mutex<Option<Weak<ProgressBar>>> {
    PROGRESS_BAR.get_or_init(|| Mutex::new(None))
}

impl Progress {
    /// Create a new global progress bar with the given number of steps.
    /// Returns a no-op instance when the log level is below `Info`.
    pub fn new(items: u64, level: LevelFilter) -> Progress {
        if level < LevelFilter::Info {
            return Progress { inner: None };
        }

        // Create the progress bar
        let p = Arc::new(ProgressBar::new(items));
        let template = format!(
            "{}{}{} {}",
            style("[").white().dim(),
            "{spinner:.cyan} {elapsed:>3}",
            style("]").white().dim(),
            "{bar:25.cyan/black} {pos:>2}/{len} {msg:.bold}",
        );
        // The template is a fixed format string with valid indicatif
        // placeholders; this can only fail if the template syntax itself
        // is wrong, which would be caught by tests.
        p.set_style(
            ProgressStyle::default_bar()
                .template(&template)
                .expect("invalid progress bar template")
                .progress_chars("━╸━"),
        );
        p.enable_steady_tick(Duration::from_millis(80));

        // A poisoned mutex means a prior holder panicked, which is
        // unrecoverable, so expect() is appropriate here and below.
        *progress_bar().lock().expect("progress bar mutex poisoned") = Some(Arc::downgrade(&p));

        Progress { inner: Some(p) }
    }

    /// Obtain the current global progress bar, if one is active.
    pub fn get() -> Option<Arc<ProgressBar>> {
        progress_bar()
            .lock()
            .expect("progress bar mutex poisoned")
            .as_ref()?
            .upgrade()
    }

    /// Finish and remove the global progress bar.
    pub fn reset(self) {
        if let Some(p) = self.inner {
            p.finish()
        }
        *progress_bar().lock().expect("progress bar mutex poisoned") = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn progress_success() {
        let p = Progress::new(10, LevelFilter::Info);
        assert!(Progress::get().is_some());
        p.reset();
        assert!(Progress::get().is_none());
    }
}
