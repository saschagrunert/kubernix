use crate::progress::Progress;
use console::{style, Color};
use log::{set_max_level, Level, LevelFilter, Log, Metadata, Record};
use std::io::{stderr, Write};

/// The main logging faccade
pub struct Logger {
    level: LevelFilter,
}

impl Logger {
    /// Create a new logger
    pub fn new(level: LevelFilter) -> Box<Self> {
        set_max_level(LevelFilter::Trace);
        Self { level }.into()
    }

    /// Log an error message
    pub fn error(msg: &str) {
        Self {
            level: LevelFilter::Error,
        }
        .log(
            &Record::builder()
                .args(format_args!("{}", msg))
                .level(Level::Error)
                .build(),
        );
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

#[cfg(test)]
pub mod tests {
    use super::*;
    use log::{MetadataBuilder, Record};

    #[test]
    fn logger_success() {
        let l = Logger::new(LevelFilter::Info);
        let record = Record::builder()
            .args(format_args!("Error!"))
            .level(Level::Error)
            .build();
        l.log(&record);
        let err_metadata = MetadataBuilder::new().level(Level::Error).build();
        assert!(l.enabled(&err_metadata));
        let dbg_metadata = MetadataBuilder::new().level(Level::Debug).build();
        assert!(!l.enabled(&dbg_metadata));
        l.flush();
    }
}
