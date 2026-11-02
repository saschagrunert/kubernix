//! Thread-safe global progress bar for cluster bootstrap stages.
//!
//! A single [`Progress`] instance owns the underlying [`ProgressBar`] via
//! `Arc`. Other parts of the codebase (e.g. the logger) can obtain a
//! weak reference through [`Progress::get`] to update or print alongside
//! the bar without taking ownership.

use crate::config::LogFormat;
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
    /// Returns a no-op instance when the log level is below `Info` or the
    /// log format is JSON, which would interleave with the bar.
    pub fn new(items: u64, level: LevelFilter, format: LogFormat) -> Progress {
        if level < LevelFilter::Info || format == LogFormat::Json {
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
    pub fn reset(mut self) {
        if let Some(p) = self.inner.take() {
            p.finish();
            Self::unregister(&p);
        }
    }

    /// Remove the bar from the global state if it is the active one.
    fn unregister(p: &Arc<ProgressBar>) {
        let mut global = progress_bar().lock().expect("progress bar mutex poisoned");
        if global.as_ref().is_none_or(|w| w.ptr_eq(&Arc::downgrade(p))) {
            *global = None;
        }
    }
}

impl Drop for Progress {
    /// Clear an unfinished bar, for example on an early error, so that it
    /// does not stay on screen above the error output.
    fn drop(&mut self) {
        if let Some(p) = self.inner.take() {
            p.finish_and_clear();
            Self::unregister(&p);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn progress_success() {
        let p = Progress::new(10, LevelFilter::Info, LogFormat::Text);
        assert!(Progress::get().is_some());
        p.reset();
        assert!(Progress::get().is_none());

        // Dropping an unfinished bar unregisters it as well
        let p = Progress::new(10, LevelFilter::Info, LogFormat::Text);
        assert!(Progress::get().is_some());
        drop(p);
        assert!(Progress::get().is_none());
    }

    #[test]
    fn progress_disabled_for_json() {
        let p = Progress::new(10, LevelFilter::Info, LogFormat::Json);
        assert!(p.inner.is_none());
    }
}
