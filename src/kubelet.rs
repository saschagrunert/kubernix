use crate::{
    config::Config,
    kubeconfig::KubeConfig,
    network::Network,
    pki::Pki,
    process::{Process, Startable, Stoppable},
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
    ) -> Fallible<Startable> {
        info!("Starting Kubelet");

        let dir = config.root().join("kubelet");
        create_dir_all(&dir)?;

        let yml = format!(
            include_str!("assets/kubelet.yml"),
            pki.ca().cert().display(),
            network.dns()?,
            network.crio_cidr(),
            pki.kubelet().cert().display(),
            pki.kubelet().key().display(),
        );
        let yml_file = dir.join("config.yml");
        fs::write(&yml_file, yml)?;

        let mut process = Process::start(
            config,
            &dir,
            "kubelet",
            &[
                &format!("--config={}", yml_file.display()),
                &format!("--root-dir={}", dir.join("run").display()),
                "--container-runtime=remote",
                &format!(
                    "--container-runtime-endpoint={}",
                    network.crio_socket().to_socket_string()
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
