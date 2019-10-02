//! Configuration related structures
use derive_builder::Builder;
use failure::Fallible;
use ipnetwork::IpNetwork;
use serde::Deserialize;
use std::{
    fs::{canonicalize, create_dir_all, remove_dir_all},
    net::IpAddr,
    path::{Path, PathBuf},
};

#[derive(Builder, Deserialize)]
#[serde(default, rename_all = "kebab-case")]
#[builder(default, setter(into))]
/// The global configuration
pub struct Config {
    /// The root path during runtime
    root: PathBuf,

    /// The logging level of the application
    log_level: String,

    /// Container Networking CIDR for CRI-O
    crio_cidr: IpNetwork,

    /// Cluster CIDR
    cluster_cidr: IpNetwork,

    /// Service CIDR
    service_cidr: IpNetwork,

    /// Cluster wide DNS address
    cluster_dns: IpAddr,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            root: PathBuf::from("kubernix"),
            log_level: "info".to_owned(),
            crio_cidr: "10.100.0.0/16".parse().unwrap(),
            cluster_cidr: "10.200.0.0/16".parse().unwrap(),
            service_cidr: "10.50.0.0/24".parse().unwrap(),
            cluster_dns: "10.50.0.10".parse().unwrap(),
        }
    }
}

impl Config {
    /// Prepare the configuration, which is a necessary step
    pub fn prepare(&mut self) -> Fallible<()> {
        // Remove the root if already existing
        if self.root().exists() {
            remove_dir_all(&self.root())?;
        }
        create_dir_all(&self.root())?;
        self.root = canonicalize(&self.root).unwrap();
        Ok(())
    }
}

impl Config {
    /// Retrieve the root path
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Retrieve the log level
    pub fn log_level(&self) -> &str {
        &self.log_level
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

    /// Retrieve the Cluster DNS address
    pub fn cluster_dns(&self) -> &IpAddr {
        &self.cluster_dns
    }
}
