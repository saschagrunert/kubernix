use crate::{config::LogFormat, progress::Progress};
use console::{Color, style};
use log::{Level, LevelFilter, Log, Metadata, Record, set_max_level};
use std::io::{IsTerminal, Write, stderr};

/// The main logging facade
pub struct Logger {
    level: LevelFilter,
    format: LogFormat,
}

impl Logger {
    /// Create a new logger
    pub fn new(level: LevelFilter, format: LogFormat) -> Box<Self> {
        set_max_level(LevelFilter::Trace);
        Self { level, format }.into()
    }

    /// Log an error message directly to stderr.
    ///
    /// This is used for fatal errors before the global logger is
    /// configured, so it writes plain text regardless of format settings.
    pub fn error(msg: &str) {
        writeln!(stderr(), "[ERROR] {}", msg).ok();
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

        match self.format {
            LogFormat::Json => self.log_json(record),
            LogFormat::Text => self.log_text(record),
        }
    }

    fn flush(&self) {}
}

impl Logger {
    fn log_json(&self, record: &Record<'_>) {
        let level = record.metadata().level().as_str().to_lowercase();
        let message = Self::escape_json(&record.args().to_string());
        let target = Self::escape_json(record.target());
        let line = format!(
            r#"{{"level":"{}","message":"{}","target":"{}"}}"#,
            level, message, target,
        );
        writeln!(stderr(), "{}", line).ok();
    }

    /// Escape a string for safe embedding in a JSON value.
    fn escape_json(s: &str) -> String {
        let mut out = String::with_capacity(s.len());
        for ch in s.chars() {
            match ch {
                '\\' => out.push_str("\\\\"),
                '"' => out.push_str("\\\""),
                '\n' => out.push_str("\\n"),
                '\r' => out.push_str("\\r"),
                '\t' => out.push_str("\\t"),
                c if c.is_control() => {
                    // Unicode escape for other control chars
                    for unit in c.encode_utf16(&mut [0; 2]) {
                        out.push_str(&format!("\\u{:04x}", unit));
                    }
                }
                c => out.push(c),
            }
        }
        out
    }

    fn log_text(&self, record: &Record<'_>) {
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
            if stderr().is_terminal() {
                if level != Level::Info {
                    pb.println(&msg);
                } else {
                    pb.inc(1);
                    pb.set_message(record.args().to_string());
                }
            } else {
                if level == Level::Info {
                    pb.inc(1);
                }
                writeln!(stderr(), "{}", msg).ok();
            }
        } else {
            writeln!(stderr(), "{}", msg).ok();
        }
    }
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use log::{MetadataBuilder, Record};

    #[test]
    fn logger_text_success() {
        let l = Logger::new(LevelFilter::Info, LogFormat::Text);
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

    #[test]
    fn logger_json_success() {
        let l = Logger::new(LevelFilter::Info, LogFormat::Json);
        let record = Record::builder()
            .args(format_args!("test message"))
            .level(Level::Info)
            .build();
        l.log(&record);
        l.flush();
    }
}
