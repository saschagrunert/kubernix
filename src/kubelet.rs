use crate::{
    config::Config,
    crio::Crio,
    kubeconfig::KubeConfig,
    network::Network,
    pki::Pki,
    process::{Process, ProcessState, Stoppable},
};
use failure::Fallible;
use log::info;
use std::fs::{self, create_dir_all};

pub struct Kubelet {
    process: Process,
}

impl Kubelet {
    pub fn start(
        config: &Config,
        network: &Network,
        pki: &Pki,
        kubeconfig: &KubeConfig,
    ) -> ProcessState {
        info!("Starting Kubelet");

        let dir = config.root().join("kubelet");
        create_dir_all(&dir)?;

        let yml = format!(
            include_str!("assets/kubelet.yml"),
            ca = pki.ca().cert().display(),
            dns = network.dns()?,
            cidr = network.crio_cidr(),
            cert = pki.kubelet().cert().display(),
            key = pki.kubelet().key().display(),
        );
        let cfg = dir.join("config.yml");

        if !cfg.exists() {
            fs::write(&cfg, yml)?;
        }

        let mut process = Process::start(
            &dir,
            "kubelet",
            &[
                &format!("--config={}", cfg.display()),
                &format!("--root-dir={}", dir.join("run").display()),
                "--container-runtime=remote",
                &format!(
                    "--container-runtime-endpoint={}",
                    Crio::socket(config).to_socket_string(),
                ),
                &format!("--kubeconfig={}", kubeconfig.kubelet().display()),
                "--v=2",
            ],
        )?;

        process.wait_ready("Successfully registered node")?;
        info!("Kubelet is ready");
        Ok(Box::new(Kubelet { process }))
    }
}

impl Stoppable for Kubelet {
    fn stop(&mut self) -> Fallible<()> {
        self.process.stop()
    }
}
