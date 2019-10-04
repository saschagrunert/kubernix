//! Configuration related structures
use derive_builder::Builder;
use failure::{format_err, Fallible};
use getset::Getters;
use ipnetwork::IpNetwork;
use lazy_static::lazy_static;
use log::LevelFilter;
use serde::{Deserialize, Serialize};
use serde_yaml::Value;
use std::{
    error::Error,
    fmt::Debug,
    fs::{self, canonicalize, create_dir_all, read_to_string},
    path::PathBuf,
    str::FromStr,
};
use toml;

#[derive(Builder, Clone, Debug, Deserialize, Getters, Serialize)]
#[serde(default, rename_all = "kebab-case")]
#[builder(default, setter(into))]
/// The global configuration
pub struct Config {
    /// The root path during runtime
    #[get = "pub"]
    root: PathBuf,

    /// The logging level of the application
    #[get = "pub"]
    log_level: LevelFilter,

    /// Container Networking CIDR for CRI-O
    #[get = "pub"]
    crio_cidr: IpNetwork,

    /// Cluster CIDR
    #[get = "pub"]
    cluster_cidr: IpNetwork,

    /// Service CIDR
    #[get = "pub"]
    service_cidr: IpNetwork,
}

impl Default for Config {
    fn default() -> Self {
        DEFAULT_CONFIG.clone()
    }
}

lazy_static! {
    static ref DEFAULT_CONFIG: Config = {
        /// Parse an internal value from a YAML Value.
        fn parse_from_yaml<T>(value: &Value, key: &str) -> Fallible<T>
        where
            T: FromStr,
            <T as FromStr>::Err: Debug + Error + Send + Sync,
        {
            value
                .get("args")
                .ok_or_else(|| format_err!("Unable to get args"))?
                .as_sequence()
                .ok_or_else(|| format_err!("Unable to get sequence"))?
                .iter()
                .find(|x| x.get(key).is_some())
                .ok_or_else(|| format_err!("Unable to find {}", key))?
                .get(key)
                .ok_or_else(|| format_err!("Unable to get {}", key))?
                .get("default_value")
                .ok_or_else(|| format_err!("Unable to get default value"))?
                .as_str()
                .ok_or_else(|| format_err!("Unable to get string reference"))?
                .parse().map_err(|e| format_err!("Unable to parse value: {}", e))
        }
        let yaml = serde_yaml::from_str(include_str!("cli.yaml")).unwrap();
        Config {
            root: parse_from_yaml(&yaml, "root").unwrap(),
            log_level: parse_from_yaml(&yaml, "log-level").unwrap(),
            crio_cidr: parse_from_yaml(&yaml, "crio-cidr").unwrap(),
            cluster_cidr: parse_from_yaml(&yaml, "cluster-cidr").unwrap(),
            service_cidr: parse_from_yaml(&yaml, "service-cidr").unwrap(),
        }
    };
}

impl Config {
    const FILENAME: &'static str = "kubernix.toml";

    /// Make the configs root path absolute
    pub fn canonicalize_root(&mut self) -> Fallible<()> {
        self.root = canonicalize(self.root())
            .map_err(|e| format_err!("Unable to canonicalize config root directory: {}", e))?;
        Ok(())
    }

    /// Write the current configuration to the internal set root path
    pub fn to_file(&self) -> Fallible<()> {
        create_dir_all(self.root())?;
        fs::write(self.root().join(Self::FILENAME), toml::to_string(&self)?)
            .map_err(|e| format_err!("Unable to write configuration to file: {}", e))?;
        Ok(())
    }

    /// Read the configuration from the internal set root path
    pub fn update_from_file(&mut self) -> Fallible<()> {
        let file = self.root().join(Self::FILENAME);
        *self = toml::from_str(&read_to_string(&file).map_err(|e| {
            format_err!(
                "Unable to read configuration file '{}': {}",
                file.display(),
                e
            )
        })?)
        .map_err(|e| format_err!("Unable to load config file '{}': {}", file.display(), e))?;
        Ok(())
    }
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use std::path::Path;
    use tempfile::tempdir;

    pub fn test_config() -> Fallible<Config> {
        let mut c = ConfigBuilder::default()
            .root(tempdir()?.into_path())
            .build()
            .map_err(|e| format_err!("Unable to build config: {}", e))?;
        c.canonicalize_root()?;
        Ok(c)
    }

    pub fn test_config_wrong_root() -> Fallible<Config> {
        let mut c = test_config()?;
        c.root = Path::new("/").join("proc");
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
        assert_eq!(c.log_level(), &LevelFilter::Debug);
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
        assert_eq!(c.log_level(), &LevelFilter::Warn);
        Ok(())
    }
}
