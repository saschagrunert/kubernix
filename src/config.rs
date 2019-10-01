//! Configuration related structures
use failure::Fallible;
use serde::Deserialize;
use std::{
    fs::{canonicalize, create_dir_all, read_to_string, remove_dir_all},
    path::PathBuf,
};
use toml;

#[derive(Clone, Deserialize, Debug)]
#[serde(default, rename_all = "kebab-case")]
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

    /// The CRI-O configuration
    pub crio: CrioConfig,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            root: PathBuf::from("kubernix"),
            log: Default::default(),
            pki: Default::default(),
            kube: Default::default(),
            crio: Default::default(),
        }
    }
}

impl Config {
    /// Creates a new `Config` instance using the parameters found in the given
    /// TOML configuration file. If the file could not be found or the file is
    /// invalid, an `Error` will be returned.
    pub fn from_file(filename: &str) -> Fallible<Self> {
        let mut config: Self = toml::from_str(&read_to_string(filename)?)?;
        if config.root.exists() {
            remove_dir_all(&config.root)?;
        }
        create_dir_all(&config.root)?;
        config.root = canonicalize(config.root)?;
        Ok(config)
    }
}

#[derive(Clone, Deserialize, Debug)]
#[serde(default, rename_all = "kebab-case")]
/// The logger configuration
pub struct LogConfig {
    /// The logging level of the application
    pub level: String,

    /// The logging directory for spawned processes
    pub dir: PathBuf,
}

impl Default for LogConfig {
    fn default() -> Self {
        LogConfig {
            level: "info".to_owned(),
            dir: PathBuf::from("log"),
        }
    }
}

#[derive(Clone, Deserialize, Debug)]
#[serde(default, rename_all = "kebab-case")]
/// The PKI configuration
pub struct PkiConfig {
    /// The directory for created certificates
    pub dir: PathBuf,
}

impl Default for PkiConfig {
    fn default() -> Self {
        PkiConfig {
            dir: PathBuf::from("pki"),
        }
    }
}

#[derive(Clone, Deserialize, Debug)]
#[serde(default, rename_all = "kebab-case")]
/// The Kube configuration
pub struct KubeConfig {
    /// The directory for created configs
    pub dir: PathBuf,

    /// Cluster CIDR
    pub cluster_cidr: String,

    /// Service CIDR
    pub service_cidr: String,

    /// Cluster wide DNS address
    pub cluster_dns: String,
}

impl Default for KubeConfig {
    fn default() -> Self {
        KubeConfig {
            dir: PathBuf::from("kube"),
            cluster_cidr: "172.200.0.0/16".to_owned(),
            service_cidr: "172.50.0.0/24".to_owned(),
            cluster_dns: "172.50.0.10".to_owned(),
        }
    }
}

#[derive(Clone, Deserialize, Debug)]
#[serde(default, rename_all = "kebab-case")]
/// The CRI-O configuration
pub struct CrioConfig {
    /// The directory for CRI-O
    pub dir: PathBuf,

    /// Container Networking CIDR
    pub cidr: String,
}

impl Default for CrioConfig {
    fn default() -> Self {
        CrioConfig {
            dir: PathBuf::from("crio"),
            cidr: "172.100.0.0/16".to_owned(),
        }
    }
}
