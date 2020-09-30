use crate::{
    config::Config,
    encryptionconfig::EncryptionConfig,
    kubectl::Kubectl,
    network::Network,
    pki::Pki,
    process::{Process, ProcessState, Stoppable},
};
use anyhow::{Context, Result};
use log::debug;
use std::{
    fs::{self, create_dir_all},
    path::Path,
};

pub struct ApiServer {
    process: Process,
}

impl ApiServer {
    pub fn start(
        config: &Config,
        network: &Network,
        pki: &Pki,
        encryptionconfig: &EncryptionConfig,
        kubectl: &Kubectl,
    ) -> ProcessState {
        let dir = config.root().join("apiserver");
        create_dir_all(&dir)?;

        let mut process = Process::start(
            &dir,
            "API Server",
            "kube-apiserver",
            &[
                "--allow-privileged=true",
                "--audit-log-maxage=30",
                "--audit-log-maxbackup=3",
                "--audit-log-maxsize=100",
                &format!("--audit-log-path={}", dir.join("audit.log").display()),
                "--authorization-mode=Node,RBAC",
                "--bind-address=0.0.0.0",
                &format!("--client-ca-file={}", pki.ca().cert().display()),
                &format!("--etcd-cafile={}", pki.ca().cert().display()),
                &format!("--etcd-certfile={}", pki.apiserver().cert().display()),
                &format!("--etcd-keyfile={}", pki.apiserver().key().display()),
                &format!("--etcd-servers=https://{}", network.etcd_client()),
                "--event-ttl=1h",
                &format!(
                    "--encryption-provider-config={}",
                    encryptionconfig.path().display()
                ),
                &format!(
                    "--kubelet-certificate-authority={}",
                    pki.ca().cert().display()
                ),
                &format!(
                    "--kubelet-client-certificate={}",
                    pki.apiserver().cert().display()
                ),
                &format!("--kubelet-client-key={}", pki.apiserver().key().display()),
                "--runtime-config=api/all=true",
                &format!(
                    "--service-account-key-file={}",
                    pki.service_account().cert().display()
                ),
                &format!("--service-cluster-ip-range={}", network.service_cidr()),
                &format!("--tls-cert-file={}", pki.apiserver().cert().display()),
                &format!("--tls-private-key-file={}", pki.apiserver().key().display()),
                "--v=2",
            ],
        )?;

        process.wait_ready("sending update to cc")?;
        Self::setup_rbac(&dir, kubectl)?;
        Ok(Box::new(Self { process }))
    }

    fn setup_rbac(dir: &Path, kubectl: &Kubectl) -> Result<()> {
        debug!("Creating API Server RBAC rule for kubelet");
        let file = dir.join("rbac.yml");

        if !file.exists() {
            fs::write(&file, include_str!("assets/apiserver.yml"))?;
        }

        kubectl
            .apply(&file)
            .context("Unable to deploy API server RBAC rules")?;

        debug!("API Server RBAC rule created");
        Ok(())
    }
}

impl Stoppable for ApiServer {
    fn stop(&mut self) -> Result<()> {
        self.process.stop()
    }
}
