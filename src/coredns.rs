use crate::{config::Config, kubeconfig::KubeConfig, kubectl::Kubectl, network::Network};
use anyhow::{Context, Result};
use log::{debug, info};
use std::{
    fs::{self, create_dir_all},
    thread::sleep,
    time::Duration,
};

pub struct CoreDNS;

impl CoreDNS {
    pub fn apply(config: &Config, network: &Network, kubeconfig: &KubeConfig) -> Result<()> {
        info!("Deploying CoreDNS and waiting to be ready");

        let dir = config.root().join("coredns");
        create_dir_all(&dir)?;

        let yml = format!(include_str!("assets/coredns.yml"), network.dns()?);
        let file = dir.join("coredns.yml");

        if !file.exists() {
            fs::write(&file, yml)?;
        }

        Kubectl::apply(kubeconfig.admin(), &file).context("Unable to deploy CoreDNS")?;

        // Wait for CoreDNS to be ready
        debug!("Waiting for CoreDNS to be ready");
        loop {
            let output = Kubectl::execute(
                kubeconfig.admin(),
                &[
                    "get",
                    "pods",
                    "-n=kube-system",
                    "-l=k8s-app=coredns",
                    "--no-headers",
                ],
            )?;
            let stdout = String::from_utf8(output.stdout)?;
            if let Some(status) = stdout.split_whitespace().nth(1) {
                debug!("CoreDNS status: {}", status);
                if stdout.contains("1/1") {
                    debug!("CoreDNS ready");
                    break;
                }
            } else {
                debug!("CoreDNS status not available right now")
            }
            sleep(Duration::from_secs(2));
        }

        info!("CoreDNS deployed");
        Ok(())
    }
}
