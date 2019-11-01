use console::{style, Color};
use indicatif::ProgressBar;
use lazy_static::lazy_static;
use log::{Level, LevelFilter, Log, Metadata, Record};
use parking_lot::RwLock;
use std::{
    io::{stderr, Write},
    sync::{Arc, Weak},
};

lazy_static! {
    static ref PROGRESS_BAR: RwLock<Option<Weak<ProgressBar>>> = RwLock::new(None);
}

/// The basic logger
pub struct Logger {
    level: LevelFilter,
}

impl Logger {
    pub fn new(level: LevelFilter) -> Box<Self> {
        Logger { level }.into()
    }
}

impl Log for Logger {
    fn enabled(&self, metadata: &Metadata<'_>) -> bool {
        metadata.level() <= self.level
    }

    fn log(&self, record: &Record<'_>) {
        if !self.enabled(record.metadata()) {
            return;
        }

        let level = record.metadata().level();
        let (level_name, level_color) = match level {
            Level::Error => ("ERROR", Color::Red),
            Level::Warn => ("WARN ", Color::Yellow),
            Level::Info => ("INFO ", Color::Green),
            Level::Debug => ("DEBUG", Color::Cyan),
            Level::Trace => ("TRACE", Color::Magenta),
        };
        let msg = format!(
            "{}{}{} {}",
            style("[").white().dim(),
            style(level_name).fg(level_color),
            style("]").white().dim(),
            style(record.args()),
        );

        if let Some(pb) = get_progress_bar() {
            if level != Level::Info {
                pb.println(msg);
            } else {
                pb.inc(1);
                pb.set_message(&record.args().to_string());
            }
        } else {
            writeln!(stderr(), "{}", msg).ok();
        }
    }

    fn flush(&self) {}
}

pub fn set_progress_bar(pb: &Arc<ProgressBar>) {
    *PROGRESS_BAR.write() = Some(Arc::downgrade(pb));
}

pub fn reset_progress_bar(pb: Option<Arc<ProgressBar>>) {
    if let Some(p) = pb {
        p.finish()
    }
    *PROGRESS_BAR.write() = None;
}

fn get_progress_bar() -> Option<Arc<ProgressBar>> {
    PROGRESS_BAR.read().as_ref()?.upgrade()
}
