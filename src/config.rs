//! Configuration related structures
use failure::Fallible;
use serde::Deserialize;
use std::{fs::read_to_string, path::PathBuf};
use toml;

#[derive(Deserialize)]
#[serde(rename_all = "kebab-case")]
/// The global configuration
pub struct Config {
    /// The root path during runtime
    pub root: PathBuf,

    /// The logger configuration
    pub log: LogConfig,

    /// The PKI configuration
    pub pki: PkiConfig,

    /// The Kube configuration
    pub kube: KubeConfig,
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
    pub dir: PathBuf,
}

#[derive(Deserialize)]
#[serde(rename_all = "kebab-case")]
/// The PKI configuration
pub struct PkiConfig {
    /// The directory for created certificates
    pub dir: PathBuf,
}

#[derive(Deserialize)]
#[serde(rename_all = "kebab-case")]
/// The Kube configuration
pub struct KubeConfig {
    /// The directory for created configs
    pub dir: PathBuf,
}
