//! Configuration related structures
use crate::{podman::Podman, system::System};
use anyhow::{Context, Result};
use clap::{AppSettings, Clap};
use ipnetwork::Ipv4Network;
use log::LevelFilter;
use serde::{Deserialize, Serialize};
use std::{
    fs::{self, canonicalize, create_dir_all, read_to_string},
    path::{Path, PathBuf},
};
use toml;

#[derive(Clap, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
#[clap(
    after_help("More info at: https://github.com/saschagrunert/kubernix"),
    author("Sascha Grunert <mail@saschagrunert.de>"),
    global_setting(AppSettings::ColoredHelp)
)]
/// The global configuration
pub struct Config {
    #[clap(subcommand)]
    /// All available subcommands
    subcommand: Option<SubCommand>,

    #[clap(
        default_value("kubernix-run"),
        env("KUBERNIX_RUN"),
        global(true),
        long("root"),
        short("r"),
        value_name("PATH")
    )]
    /// Path where all the runtime data is stored
    root: PathBuf,

    #[clap(
        default_value("info"),
        env("KUBERNIX_LOG_LEVEL"),
        long("log-level"),
        possible_values(&["trace", "debug", "info", "warn", "error", "off"]),
        short("l"),
        value_name("LEVEL")
    )]
    /// The logging level of the application
    log_level: LevelFilter,

    #[clap(
        default_value("10.10.0.0/16"),
        env("KUBERNIX_CIDR"),
        long("cidr"),
        short("c"),
        value_name("CIDR")
    )]
    /// The CIDR used for the cluster
    cidr: Ipv4Network,

    #[clap(
        env("KUBERNIX_OVERLAY"),
        long("overlay"),
        short("o"),
        value_name("PATH")
    )]
    /// The Nix package overlay to be used
    overlay: Option<PathBuf>,

    #[clap(
        env("KUBERNIX_PACKAGES"),
        long("packages"),
        multiple(true),
        short("p"),
        value_name("PACKAGE")
    )]
    /// Additional dependencies to be added to the environment
    packages: Vec<String>,

    #[clap(env("KUBERNIX_SHELL"), long("shell"), short("s"), value_name("SHELL"))]
    /// The shell executable to be used, defaults to $SHELL, fallback is `sh`
    shell: Option<String>,

    #[clap(
        default_value("1"),
        env("KUBERNIX_NODES"),
        long("nodes"),
        short("n"),
        value_name("NODES")
    )]
    /// The number of nodes to be registered
    nodes: u8,

    #[clap(
        env("KUBERNIX_CONTAINER_RUNTIME"),
        long("container-runtime"),
        default_value(Podman::EXECUTABLE),
        requires("nodes"),
        short("u"),
        value_name("RUNTIME")
    )]
    /// The container runtime to be used for the nodes, irrelevant if `nodes` equals to `1`
    container_runtime: String,

    #[clap(
        conflicts_with("shell"),
        env("KUBERNIX_NO_SHELL"),
        long("no-shell"),
        short("e"),
        takes_value(false)
    )]
    /// Do not spawn an interactive shell after bootstrap
    no_shell: bool,
}

/// Possible subcommands
#[derive(Clap, Deserialize, Serialize)]
pub enum SubCommand {
    /// Spawn an additional shell session
    #[clap(name("shell"))]
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

    /// Getter for root
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Getter for subcommand
    pub fn subcommand(&self) -> &Option<SubCommand> {
        &self.subcommand
    }

    /// Getter for log_level
    pub fn log_level(&self) -> LevelFilter {
        self.log_level
    }

    /// Getter for nodes
    pub fn nodes(&self) -> u8 {
        self.nodes
    }

    /// Getter for overlay
    pub fn overlay(&self) -> &Option<PathBuf> {
        &self.overlay
    }

    /// Getter for packages
    pub fn packages(&self) -> &[String] {
        &self.packages
    }

    /// Getter for cidr
    pub fn cidr(&self) -> Ipv4Network {
        self.cidr
    }

    /// Getter for container_runtime
    pub fn container_runtime(&self) -> &str {
        &self.container_runtime
    }

    /// Getter for no_shell
    pub fn no_shell(&self) -> bool {
        self.no_shell
    }

    /// Make the configs root path absolute
    pub fn canonicalize_root(&mut self) -> Result<()> {
        self.create_root_dir()?;
        self.root =
            canonicalize(self.root()).context("Unable to canonicalize config root directory")?;
        Ok(())
    }

    /// Write the current configuration to the internal set root path
    pub fn to_file(&self) -> Result<()> {
        self.create_root_dir()?;
        fs::write(self.root().join(Self::FILENAME), toml::to_string(&self)?)
            .context("Unable to write configuration to file")?;
        Ok(())
    }

    /// Read the configuration from the internal set root path
    /// If not existing, write the current configuration to the path.
    pub fn try_load_file(&mut self) -> Result<()> {
        let file = self.root().join(Self::FILENAME);
        if file.exists() {
            *self = toml::from_str(&read_to_string(&file).with_context(|| {
                format!(
                    "Unable to read expected configuration file '{}'",
                    file.display(),
                )
            })?)
            .with_context(|| format!("Unable to load config file '{}'", file.display()))?;
        } else {
            self.to_file()?;
        }
        Ok(())
    }

    /// Return the set shell as result type
    pub fn shell_ok(&self) -> Result<String> {
        let shell = self.shell.as_ref().context("No shell set")?;
        Ok(shell.into())
    }

    /// Returns true if multi node support is enabled
    pub fn multi_node(&self) -> bool {
        self.nodes() > 1
    }

    fn create_root_dir(&self) -> Result<()> {
        create_dir_all(self.root()).context("Unable to create root directory")
    }
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use std::path::Path;
    use tempfile::tempdir;

    pub fn test_config() -> Result<Config> {
        let mut c = Config::default();
        c.root = tempdir()?.into_path();
        c.canonicalize_root()?;
        Ok(c)
    }

    pub fn test_config_wrong_root() -> Result<Config> {
        let mut c = test_config()?;
        c.root = Path::new("/").join("proc");
        Ok(c)
    }

    pub fn test_config_wrong_cidr() -> Result<Config> {
        let mut c = test_config()?;
        c.cidr = "10.0.0.1/25".parse()?;
        Ok(c)
    }

    #[test]
    fn canonicalize_root_success() -> Result<()> {
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
    fn to_file_success() -> Result<()> {
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
    fn try_load_file_success() -> Result<()> {
        let mut c = Config::default();
        c.root = tempdir()?.into_path();
        fs::write(
            c.root.join(Config::FILENAME),
            r#"
cidr = "1.1.1.1/16"
container-runtime = "podman"
log-level = "DEBUG"
no-shell = false
nodes = 1
packages = []
root = "root"
            "#,
        )?;
        c.try_load_file()?;
        assert_eq!(c.root(), Path::new("root"));
        assert_eq!(c.log_level(), LevelFilter::Debug);
        assert_eq!(&c.cidr().to_string(), "1.1.1.1/16");
        Ok(())
    }

    #[test]
    fn try_load_file_failure() -> Result<()> {
        let mut c = Config::default();
        c.root = tempdir()?.into_path();
        fs::write(c.root.join(Config::FILENAME), "invalid")?;
        assert!(c.try_load_file().is_err());
        Ok(())
    }
}
