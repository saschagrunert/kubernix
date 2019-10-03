//! Configuration related structures
use derive_builder::Builder;
use failure::{format_err, Fallible};
use ipnetwork::IpNetwork;
use log::LevelFilter;
use serde::{Deserialize, Serialize};
use std::{
    fs::{self, canonicalize, create_dir_all, read_to_string},
    path::{Path, PathBuf},
};
use toml;

#[derive(Builder, Deserialize, Serialize)]
#[serde(default, rename_all = "kebab-case")]
#[builder(default, setter(into))]
/// The global configuration
pub struct Config {
    /// The root path during runtime
    root: PathBuf,

    /// The logging level of the application
    log_level: LevelFilter,

    /// Container Networking CIDR for CRI-O
    crio_cidr: IpNetwork,

    /// Cluster CIDR
    cluster_cidr: IpNetwork,

    /// Service CIDR
    service_cidr: IpNetwork,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            root: PathBuf::from("kubernix"),
            log_level: "info".parse().unwrap(),
            crio_cidr: "10.100.0.0/16".parse().unwrap(),
            cluster_cidr: "10.200.0.0/16".parse().unwrap(),
            service_cidr: "10.50.0.0/24".parse().unwrap(),
        }
    }
}

impl Config {
    const FILENAME: &'static str = "kubernix.toml";

    /// Make the configs root path absolute
    pub fn canonicalize_root(&mut self) -> Fallible<()> {
        self.root = canonicalize(self.root())?;
        Ok(())
    }

    /// Write the current configuration to the internal set root path
    pub fn to_file(&self) -> Fallible<()> {
        create_dir_all(self.root())?;
        fs::write(self.root().join(Self::FILENAME), toml::to_string(&self)?)?;
        Ok(())
    }

    /// Read the configuration from the internal set root path
    pub fn update_from_file(&mut self) -> Fallible<()> {
        let file = self.root().join(Self::FILENAME);
        *self = toml::from_str(&read_to_string(&file)?)
            .map_err(|e| format_err!("Unable to load config file '{}': {}", file.display(), e))?;
        Ok(())
    }

    /// Retrieve the root path
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Retrieve the log level
    pub fn log_level(&self) -> LevelFilter {
        self.log_level
    }

    /// Retrieve the CRI-O container CIDR
    pub fn crio_cidr(&self) -> &IpNetwork {
        &self.crio_cidr
    }

    /// Retrieve the cluster CIDR
    pub fn cluster_cidr(&self) -> &IpNetwork {
        &self.cluster_cidr
    }

    /// Retrieve the service CIDR
    pub fn service_cidr(&self) -> &IpNetwork {
        &self.service_cidr
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

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
    fn update_from_file_success() -> Fallible<()> {
        let mut c = Config::default();
        c.root = tempdir()?.into_path();
        fs::write(
            c.root.join(Config::FILENAME),
            r#"
root = "root"
log-level = "DEBUG"
crio-cidr = "1.1.1.1/16"
cluster-cidr = "2.2.2.2/16"
service-cidr = "3.3.3.3/24"
            "#,
        )?;
        c.update_from_file()?;
        assert_eq!(c.root(), Path::new("root"));
        assert_eq!(c.log_level(), LevelFilter::Debug);
        assert_eq!(c.crio_cidr().to_string(), "1.1.1.1/16");
        assert_eq!(c.cluster_cidr().to_string(), "2.2.2.2/16");
        assert_eq!(c.service_cidr().to_string(), "3.3.3.3/24");
        Ok(())
    }

    #[test]
    fn update_from_file_failure() -> Fallible<()> {
        let mut c = Config::default();
        c.root = tempdir()?.into_path();
        fs::write(c.root.join(Config::FILENAME), "invalid")?;
        assert!(c.update_from_file().is_err());
        Ok(())
    }

    #[test]
    fn builder_success() -> Fallible<()> {
        let c = ConfigBuilder::default()
            .root("root")
            .log_level(LevelFilter::Warn)
            .build()
            .map_err(|e| format_err!("Unable to build config: {}", e))?;
        assert_eq!(c.root(), Path::new("root"));
        assert_eq!(c.log_level(), LevelFilter::Warn);
        Ok(())
    }
}
