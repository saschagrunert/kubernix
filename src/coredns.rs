//! CoreDNS addon deployment for the cluster.

use crate::{config::Config, kubectl::Kubectl, network::Network};
use anyhow::{Context, Result};
use log::info;
use std::{
    fs::{self, create_dir_all},
    net::Ipv4Addr,
};

/// Deploys the CoreDNS addon to the cluster.
pub struct CoreDns;

impl CoreDns {
    /// Render the CoreDNS manifest, apply it, and wait for the pod to be ready.
    pub fn apply(config: &Config, network: &Network, kubectl: &Kubectl) -> Result<()> {
        info!("Deploying CoreDNS and waiting to be ready");

        let dir = config.root().join("coredns");
        create_dir_all(&dir)?;

        let yml = Self::render(network.dns()?);
        let file = dir.join("coredns.yml");

        if !file.exists() {
            fs::write(&file, yml)?;
        }

        kubectl.apply(&file).context("Unable to deploy CoreDNS")?;
        kubectl.wait_ready("coredns")?;
        info!("CoreDNS deployed");
        Ok(())
    }

    fn render(dns: Ipv4Addr) -> String {
        format!(include_str!("assets/coredns.yml"), dns)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_contains_dns_ip() {
        let ip = Ipv4Addr::new(10, 10, 1, 2);
        let yml = CoreDns::render(ip);
        assert!(yml.contains("clusterIP: 10.10.1.2"));
        assert!(yml.contains("k8s-app: coredns"));
    }
}
