use crate::{
    config::Config,
    kubeconfig::KubeConfig,
    pki::Pki,
    process::{Process, Startable, Stoppable},
};
use failure::Fallible;
use log::info;
use std::{
    fs::{self, create_dir_all},
    path::Path,
};

pub struct Kubelet {
    process: Process,
}

impl Kubelet {
    pub fn start(
        config: &Config,
        pki: &Pki,
        kubeconfig: &KubeConfig,
        socket: &Path,
    ) -> Fallible<Startable> {
        info!("Starting Kubelet");

        let dir = config.root().join("kubelet");
        create_dir_all(&dir)?;

        let yml = format!(
            include_str!("assets/kubelet.yml"),
            pki.ca.cert().display(),
            config.cluster_dns(),
            config.crio_cidr(),
            pki.kubelet.cert().display(),
            pki.kubelet.key().display()
        );
        let yml_file = dir.join("config.yml");
        fs::write(&yml_file, yml)?;

        let mut process = Process::start(
            config,
            &[
                "kubelet".to_owned(),
                format!("--config={}", yml_file.display()),
                "--container-runtime=remote".to_owned(),
                format!("--container-runtime-endpoint=unix://{}", socket.display()),
                format!("--kubeconfig={}", kubeconfig.kubelet.display()),
                "--image-pull-progress-deadline=2m".to_owned(),
                "--network-plugin=cni".to_owned(),
                "--register-node=true".to_owned(),
                "--fail-swap-on=false".to_owned(),
                "--v=2".to_owned(),
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
