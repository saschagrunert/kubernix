use crate::{config::Config, kubeconfig::KubeConfig, kubectl::Kubectl, network::Network};
use anyhow::{Context, Result};
use log::info;
use std::fs::{self, create_dir_all};

pub struct CoreDNS;

impl CoreDNS {
    pub fn apply(config: &Config, network: &Network, kubeconfig: &KubeConfig) -> Result<()> {
        info!("Deploying CoreDNS");

        let dir = config.root().join("coredns");
        create_dir_all(&dir)?;

        let yml = format!(include_str!("assets/coredns.yml"), network.dns()?);
        let file = dir.join("coredns.yml");

        if !file.exists() {
            fs::write(&file, yml)?;
        }

        Kubectl::apply(kubeconfig.admin(), &file).context("Unable to deploy CoreDNS")?;

        info!("CoreDNS deployed");
        Ok(())
    }
}
