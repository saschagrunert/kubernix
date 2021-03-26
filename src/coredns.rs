use crate::{config::Config, kubectl::Kubectl, network::Network};
use anyhow::{Context, Result};
use log::info;
use std::fs::{self, create_dir_all};

pub struct CoreDns;

impl CoreDns {
    pub fn apply(config: &Config, network: &Network, kubectl: &Kubectl) -> Result<()> {
        info!("Deploying CoreDNS and waiting to be ready");

        let dir = config.root().join("coredns");
        create_dir_all(&dir)?;

        let yml = format!(include_str!("assets/coredns.yml"), network.dns()?);
        let file = dir.join("coredns.yml");

        if !file.exists() {
            fs::write(&file, yml)?;
        }

        kubectl.apply(&file).context("Unable to deploy CoreDNS")?;
        kubectl.wait_ready("coredns")?;
        info!("CoreDNS deployed");
        Ok(())
    }
}
