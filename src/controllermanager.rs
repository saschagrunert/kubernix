use crate::{
    config::Config,
    kubeconfig::KubeConfig,
    pki::Pki,
    process::{Process, Startable, Stoppable},
};
use failure::Fallible;
use log::info;

pub struct ControllerManager {
    process: Process,
}

impl ControllerManager {
    pub fn start(config: &Config, pki: &Pki, kubeconfig: &KubeConfig) -> Fallible<Startable> {
        info!("Starting Controller Manager");

        let mut process = Process::start(
            config,
            &[
                "kube-controller-manager".to_owned(),
                "--bind-address=0.0.0.0".to_owned(),
                format!("--cluster-cidr={}", config.cluster_cidr()),
                "--cluster-name=kubernetes".to_owned(),
                format!("--cluster-signing-cert-file={}", pki.ca.cert().display()),
                format!("--cluster-signing-key-file={}", pki.ca.key().display()),
                format!("--kubeconfig={}", kubeconfig.controller_manager.display()),
                "--leader-elect=false".to_owned(),
                format!("--root-ca-file={}", pki.ca.cert().display()),
                format!(
                    "--service-account-private-key-file={}",
                    pki.service_account.key().display()
                ),
                format!("--service-cluster-ip-range={}", config.service_cidr()),
                "--use-service-account-credentials=true".to_owned(),
                "--v=2".to_owned(),
            ],
        )?;

        process.wait_ready("Serving securely")?;
        info!("Controller Manager is ready");
        Ok(Box::new(ControllerManager { process }))
    }
}

impl Stoppable for ControllerManager {
    fn stop(&mut self) -> Fallible<()> {
        self.process.stop()
    }
}
