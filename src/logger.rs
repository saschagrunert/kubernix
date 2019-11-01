use crate::progress::Progress;
use console::{style, Color};
use log::{Level, LevelFilter, Log, Metadata, Record};
use std::io::{stderr, Write};

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

        if let Some(pb) = Progress::get() {
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
