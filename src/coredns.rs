use crate::{config::Config, kubeconfig::KubeConfig, kubectl::Kubectl, network::Network};
use failure::{format_err, Fallible};
use log::info;
use std::fs::{self, create_dir_all};

pub struct CoreDNS;

impl CoreDNS {
    pub fn apply(config: &Config, network: &Network, kubeconfig: &KubeConfig) -> Fallible<()> {
        info!("Deploying CoreDNS");

        let dir = config.root().join("coredns");
        create_dir_all(&dir)?;

        let yml = format!(include_str!("assets/coredns.yml"), network.dns()?);
        let file = dir.join("coredns.yml");

        if !file.exists() {
            fs::write(&file, yml)?;
        }

        Kubectl::apply(kubeconfig, &file)
            .map_err(|e| format_err!("Unable to deploy CoreDNS: {}", e))?;

        info!("CoreDNS deployed");
        Ok(())
    }
}
