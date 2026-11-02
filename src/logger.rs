//! Logging facade with text and JSON output.
//!
//! Integrates with the progress bar so that log messages are printed
//! alongside (or in place of) the progress indicator, depending on the
//! log level and terminal capabilities.

use crate::{config::LogFormat, progress::Progress};
use anyhow::{Context, Result, anyhow};
use console::{Color, style};
use log::{Level, LevelFilter, Log, Metadata, Record, set_boxed_logger, set_max_level};
use std::{
    io::{IsTerminal, Write, stderr},
    sync::RwLock,
};

/// The settings of the installed logger, which can change afterwards.
static SETTINGS: RwLock<Option<Settings>> = RwLock::new(None);

/// The level and format of the log output.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Settings {
    level: LevelFilter,
    format: LogFormat,
}

impl Settings {
    fn enabled(self, level: Level) -> bool {
        level <= self.level
    }
}

/// The main logging facade, which uses the installed settings.
pub struct Logger;

impl Logger {
    /// Install the logger globally, or change the level and format of the
    /// installed one.
    pub(crate) fn install(level: LevelFilter, format: LogFormat) -> Result<()> {
        let mut settings = SETTINGS
            .write()
            .map_err(|_| anyhow!("Unable to change the logger"))?;
        if settings.is_none() {
            set_boxed_logger(Box::new(Logger)).context("Unable to set logger")?;
        }
        *settings = Some(Settings { level, format });
        set_max_level(level);
        Ok(())
    }

    /// The settings of the installed logger.
    fn settings() -> Option<Settings> {
        SETTINGS.read().ok().and_then(|s| *s)
    }

    /// Log the fatal error which kubernix exits with, including its causes.
    ///
    /// Uses the format of the installed logger, but not its level, so that
    /// kubernix reports why it failed even with `--log-level=off`. Before
    /// the logger is installed, for example on invalid arguments, it writes
    /// plain text.
    pub fn fatal(error: &anyhow::Error) {
        let write = |format, line: &str| {
            Self::write(
                format,
                &Record::builder()
                    .args(format_args!("{}", line))
                    .level(Level::Error)
                    .target(module_path!())
                    .build(),
            )
        };
        match Self::settings().map(|s| s.format) {
            Some(LogFormat::Json) => write(LogFormat::Json, &format!("{:#}", error)),
            Some(LogFormat::Text) => {
                for line in Self::error_lines(error) {
                    write(LogFormat::Text, &line);
                }
            }
            None => {
                for line in Self::error_lines(error) {
                    writeln!(stderr(), "[ERROR] {}", line).ok();
                }
            }
        }
    }

    /// The error and its numbered causes, one per line.
    fn error_lines(error: &anyhow::Error) -> Vec<String> {
        error
            .chain()
            .enumerate()
            .map(|(i, cause)| match i {
                0 => cause.to_string(),
                i => format!("  {}: {}", i, cause),
            })
            .collect()
    }
}

impl Log for Logger {
    fn enabled(&self, metadata: &Metadata<'_>) -> bool {
        Self::settings().is_some_and(|s| s.enabled(metadata.level()))
    }

    fn log(&self, record: &Record<'_>) {
        if let Some(settings) = Self::settings()
            && settings.enabled(record.level())
        {
            Self::write(settings.format, record);
        }
    }

    fn flush(&self) {}
}

impl Logger {
    /// Write the record in the format, regardless of its level.
    fn write(format: LogFormat, record: &Record<'_>) {
        match format {
            LogFormat::Json => Self::log_json(record),
            LogFormat::Text => Self::log_text(record),
        }
    }

    fn log_json(record: &Record<'_>) {
        writeln!(stderr(), "{}", Self::json_line(record)).ok();
    }

    /// Format the record as a single line JSON object.
    fn json_line(record: &Record<'_>) -> String {
        serde_json::json!({
            "level": record.metadata().level().as_str().to_lowercase(),
            "message": record.args().to_string(),
            "target": record.target(),
        })
        .to_string()
    }

    fn log_text(record: &Record<'_>) {
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
mod tests {
    use super::*;
    use log::{MetadataBuilder, Record};

    #[test]
    fn settings_enabled() {
        let settings = Settings {
            level: LevelFilter::Info,
            format: LogFormat::Text,
        };
        assert!(settings.enabled(Level::Error));
        assert!(settings.enabled(Level::Info));
        assert!(!settings.enabled(Level::Debug));
        let off = Settings {
            level: LevelFilter::Off,
            ..settings
        };
        assert!(!off.enabled(Level::Error));
    }

    #[test]
    fn install_changes_settings() -> Result<()> {
        // Other tests may install the logger as well, so only this test
        // changes the settings to verify them
        Logger::install(LevelFilter::Off, LogFormat::Json)?;
        Logger::install(LevelFilter::Debug, LogFormat::Text)?;
        assert_eq!(
            Logger::settings(),
            Some(Settings {
                level: LevelFilter::Debug,
                format: LogFormat::Text,
            })
        );
        assert_eq!(log::max_level(), LevelFilter::Debug);
        let metadata = MetadataBuilder::new().level(Level::Debug).build();
        assert!(log::logger().enabled(&metadata));
        let metadata = MetadataBuilder::new().level(Level::Trace).build();
        assert!(!log::logger().enabled(&metadata));
        Logger::install(LevelFilter::Off, LogFormat::Text)
    }

    #[test]
    fn error_lines_number_causes() {
        let e = anyhow::anyhow!("root").context("middle").context("top");
        assert_eq!(Logger::error_lines(&e), ["top", "  1: middle", "  2: root"]);
    }

    #[test]
    fn logger_json_line() {
        let record = Record::builder()
            .args(format_args!("test message"))
            .level(Level::Info)
            .target("kubernix")
            .build();
        let line = Logger::json_line(&record);
        let json: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(
            json,
            serde_json::json!({
                "level": "info",
                "message": "test message",
                "target": "kubernix",
            })
        );

        // Line breaks in the message are escaped, one record is one line
        let record = Record::builder()
            .args(format_args!("multi\nline \"quoted\""))
            .level(Level::Warn)
            .build();
        let line = Logger::json_line(&record);
        assert!(!line.contains('\n'));
        let json: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(json["level"], "warn");
        assert_eq!(json["message"], "multi\nline \"quoted\"");
    }
}
