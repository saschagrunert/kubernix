//! Configuration related structures
use failure::Fallible;
use serde::Deserialize;
use std::fs::read_to_string;
use toml;

#[derive(Deserialize)]
/// The global configuration
pub struct Config {
    /// The logger configuration
    pub log: LogConfig,

    /// The PKI configuration
    pub pki: PkiConfig,
}

impl Config {
    /// Creates a new `Config` instance using the parameters found in the given
    /// TOML configuration file. If the file could not be found or the file is
    /// invalid, an `Error` will be returned.
    pub fn from_file(filename: &str) -> Fallible<Self> {
        Ok(toml::from_str(&read_to_string(filename)?)?)
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "kebab-case")]
/// The logger configuration
pub struct LogConfig {
    /// The logging level of the application
    pub level: String,

    /// The logging directory for spawned processes
    pub dir: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "kebab-case")]
/// The PKI configuration
pub struct PkiConfig {
    /// The directory for created certificates
    pub dir: String,
}
