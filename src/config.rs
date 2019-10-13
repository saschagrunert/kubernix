//! Configuration related structures
use crate::system::System;
use clap::{crate_version, AppSettings, Clap};
use failure::{format_err, Fallible};
use getset::{CopyGetters, Getters};
use ipnetwork::Ipv4Network;
use log::LevelFilter;
use serde::{Deserialize, Serialize};
use std::{
    fs::{self, canonicalize, create_dir_all, read_to_string},
    path::PathBuf,
};
use toml;

#[derive(Clap, Deserialize, Getters, CopyGetters, Serialize)]
#[serde(rename_all = "kebab-case")]
#[clap(
    after_help = "More info at: https://github.com/saschagrunert/kubernix",
    author = "Sascha Grunert <mail@saschagrunert.de>",
    raw(global_setting = "AppSettings::ColoredHelp"),
    raw(version = "crate_version!()")
)]
/// The global configuration
pub struct Config {
    #[get = "pub"]
    #[clap(subcommand)]
    /// All available subcommands
    subcommand: Option<SubCommand>,

    #[get = "pub"]
    #[clap(
        default_value = "kubernix-run",
        env = "KUBERNIX_RUN",
        global = true,
        long = "root",
        short = "r",
        value_name = "PATH"
    )]
    /// Path where all the runtime data is stored
    root: PathBuf,

    #[get_copy = "pub"]
    #[clap(
        env = "KUBERNIX_CONTAINER",
        long = "container",
        short = "a",
        takes_value = false
    )]
    /// Indicator that KuberNix is running inside a container
    container: bool,

    #[get_copy = "pub"]
    #[clap(
        default_value = "info",
        env = "KUBERNIX_LOG_LEVEL",
        long = "log-level",
        raw(possible_values = r#"&["trace", "debug", "info", "warn", "error", "off"]"#),
        short = "l",
        value_name = "LEVEL"
    )]
    /// The logging level of the application
    log_level: LevelFilter,

    #[get_copy = "pub"]
    #[clap(
        default_value = "10.10.0.0/16",
        env = "KUBERNIX_CIDR",
        long = "cidr",
        short = "c",
        value_name = "CIDR"
    )]
    /// The CIDR used for the cluster
    cidr: Ipv4Network,

    #[get = "pub"]
    #[clap(
        env = "KUBERNIX_OVERLAY",
        long = "overlay",
        short = "o",
        value_name = "PATH"
    )]
    /// The Nix package overlay to be used
    overlay: Option<PathBuf>,

    #[get = "pub"]
    #[clap(
        env = "KUBERNIX_PACKAGES",
        long = "packages",
        multiple = true,
        short = "p",
        value_name = "PACKAGE"
    )]
    /// Additional dependencies to be added to the environment
    packages: Vec<String>,

    #[get = "pub"]
    #[clap(
        env = "KUBERNIX_SHELL",
        long = "shell",
        short = "s",
        value_name = "SHELL"
    )]
    /// The shell executable to be used, defaults to $SHELL, fallback is 'sh'
    shell: Option<String>,
}

/// Possible subcommands
#[derive(Clap, Deserialize, Serialize)]
pub enum SubCommand {
    /// Spawn an additional shell session
    #[clap(name = "shell")]
    Shell,
}

impl Default for Config {
    fn default() -> Self {
        let mut config = Self::parse();
        if config.shell.is_none() {
            config.shell = System::shell().ok();
        }
        config
    }
}

impl Config {
    const FILENAME: &'static str = "kubernix.toml";

    /// Make the configs root path absolute
    pub fn canonicalize_root(&mut self) -> Fallible<()> {
        self.create_root_dir()?;
        self.root = canonicalize(self.root())
            .map_err(|e| format_err!("Unable to canonicalize config root directory: {}", e))?;
        Ok(())
    }

    /// Write the current configuration to the internal set root path
    pub fn to_file(&self) -> Fallible<()> {
        self.create_root_dir()?;
        fs::write(self.root().join(Self::FILENAME), toml::to_string(&self)?)
            .map_err(|e| format_err!("Unable to write configuration to file: {}", e))?;
        Ok(())
    }

    /// Read the configuration from the internal set root path
    /// If not existing, write the current configuration to the path.
    pub fn from_or_to_file(&mut self) -> Fallible<()> {
        let file = self.root().join(Self::FILENAME);
        if file.exists() {
            *self = toml::from_str(&read_to_string(&file).map_err(|e| {
                format_err!(
                    "Unable to read expected configuration file '{}': {}",
                    file.display(),
                    e
                )
            })?)
            .map_err(|e| format_err!("Unable to load config file '{}': {}", file.display(), e))?;
        } else {
            self.to_file()?;
        }
        Ok(())
    }

    /// Return the set shell as result type
    pub fn shell_ok(&self) -> Fallible<String> {
        let shell = self
            .shell()
            .as_ref()
            .ok_or_else(|| format_err!("No shell set"))?;
        Ok(shell.to_owned())
    }

    fn create_root_dir(&self) -> Fallible<()> {
        create_dir_all(self.root())
            .map_err(|e| format_err!("Unable to create root directory: {}", e))
    }
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use std::path::Path;
    use tempfile::tempdir;

    pub fn test_config() -> Fallible<Config> {
        let mut c = Config::default();
        c.root = tempdir()?.into_path();
        c.canonicalize_root()?;
        Ok(c)
    }

    pub fn test_config_wrong_root() -> Fallible<Config> {
        let mut c = test_config()?;
        c.root = Path::new("/").join("proc");
        Ok(c)
    }

    pub fn test_config_wrong_cidr() -> Fallible<Config> {
        let mut c = test_config()?;
        c.cidr = "10.0.0.1/25".parse()?;
        Ok(c)
    }

    #[test]
    fn canonicalize_root_success() -> Fallible<()> {
        let mut c = Config::default();
        c.root = tempdir()?.into_path();
        c.canonicalize_root()
    }

    #[test]
    fn canonicalize_root_failure() {
        let mut c = Config::default();
        c.root = Path::new("/").join("proc").join("invalid");
        assert!(c.canonicalize_root().is_err())
    }

    #[test]
    fn to_file_success() -> Fallible<()> {
        let mut c = Config::default();
        c.root = tempdir()?.into_path();
        c.to_file()
    }

    #[test]
    fn to_file_failure() {
        let mut c = Config::default();
        c.root = Path::new("/").join("proc").join("invalid");
        assert!(c.to_file().is_err())
    }

    #[test]
    fn from_or_to_file_success() -> Fallible<()> {
        let mut c = Config::default();
        c.root = tempdir()?.into_path();
        fs::write(
            c.root.join(Config::FILENAME),
            r#"
root = "root"
log-level = "DEBUG"
cidr = "1.1.1.1/16"
packages = []
container = false
            "#,
        )?;
        c.from_or_to_file()?;
        assert_eq!(c.root(), Path::new("root"));
        assert_eq!(c.log_level(), LevelFilter::Debug);
        assert_eq!(&c.cidr().to_string(), "1.1.1.1/16");
        Ok(())
    }

    #[test]
    fn from_or_to_file_failure() -> Fallible<()> {
        let mut c = Config::default();
        c.root = tempdir()?.into_path();
        fs::write(c.root.join(Config::FILENAME), "invalid")?;
        assert!(c.from_or_to_file().is_err());
        Ok(())
    }
}
