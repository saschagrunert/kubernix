use crate::{
    config::Config,
    kubeconfig::KubeConfig,
    network::Network,
    process::{Process, ProcessState, Stoppable},
};
use failure::Fallible;
use log::info;
use std::fs::{self, create_dir_all};

pub struct Proxy {
    process: Process,
}

impl Proxy {
    pub fn start(config: &Config, network: &Network, kubeconfig: &KubeConfig) -> ProcessState {
        info!("Starting Proxy");

        let dir = config.root().join("proxy");
        create_dir_all(&dir)?;

        let yml = format!(
            include_str!("assets/proxy.yml"),
            kubeconfig.proxy().display(),
            network.cluster_cidr(),
        );
        let cfg = dir.join("config.yml");

        if !cfg.exists() {
            fs::write(&cfg, yml)?;
        }

        let mut process = Process::start(
            &dir,
            "proxy",
            "kube-proxy",
            &[&format!("--config={}", cfg.display())],
        )?;

        process.wait_ready("Caches are synced")?;
        info!("Proxy is ready");
        Ok(Box::new(Proxy { process }))
    }
}

impl Stoppable for Proxy {
    fn stop(&mut self) -> Fallible<()> {
        self.process.stop()
    }
}
