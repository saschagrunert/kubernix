use crate::{
    config::Config,
    encryptionconfig::EncryptionConfig,
    kubeconfig::KubeConfig,
    pki::Pki,
    process::{Process, Startable, Stoppable},
};
use failure::{bail, Fallible};
use log::{debug, info};
use std::{
    fs::{self, create_dir_all},
    net::Ipv4Addr,
    path::Path,
    process::Command,
};

pub struct APIServer {
    process: Process,
}

impl APIServer {
    pub fn start(
        config: &Config,
        ip: &str,
        pki: &Pki,
        encryptionconfig: &EncryptionConfig,
        kubeconfig: &KubeConfig,
    ) -> Fallible<Startable> {
        info!("Starting API Server");

        let dir = config.root.join("apiserver");
        create_dir_all(&dir)?;

        let mut process = Process::start(
            config,
            &[
                "kube-apiserver".to_owned(),
                format!("--advertise-address={}", ip),
                "--allow-privileged=true".to_owned(),
                "--audit-log-maxage=30".to_owned(),
                "--audit-log-maxbackup=3".to_owned(),
                "--audit-log-maxsize=100".to_owned(),
                format!("--audit-log-path={}", dir.join("audit.log").display()),
                "--authorization-mode=Node,RBAC".to_owned(),
                "--bind-address=0.0.0.0".to_owned(),
                format!("--client-ca-file={}", pki.ca.cert().display()),
                format!("--etcd-cafile={}", pki.ca.cert().display()),
                format!("--etcd-certfile={}", pki.apiserver.cert().display()),
                format!("--etcd-keyfile={}", pki.apiserver.key().display()),
                format!(
                    "--etcd-servers=https://{}:2379",
                    &Ipv4Addr::LOCALHOST.to_string(),
                ),
                "--event-ttl=1h".to_owned(),
                format!(
                    "--encryption-provider-config={}",
                    encryptionconfig.path().display()
                ),
                format!(
                    "--kubelet-certificate-authority={}",
                    pki.ca.cert().display()
                ),
                format!(
                    "--kubelet-client-certificate={}",
                    pki.apiserver.cert().display()
                ),
                format!("--kubelet-client-key={}", pki.apiserver.key().display()),
                "--kubelet-https=true".to_owned(),
                "--runtime-config=api/all".to_owned(),
                format!(
                    "--service-account-key-file={}",
                    pki.service_account.cert().display()
                ),
                format!("--service-cluster-ip-range={}", config.kube.service_cidr),
                format!("--tls-cert-file={}", pki.apiserver.cert().display()),
                format!("--tls-private-key-file={}", pki.apiserver.key().display()),
                "--v=2".to_owned(),
            ],
        )?;

        process.wait_ready("etcd ok")?;
        Self::setup_rbac(&dir, &kubeconfig.admin)?;
        info!("API Server is ready");
        Ok(Box::new(APIServer { process }))
    }

    fn setup_rbac(dir: &Path, admin_config: &Path) -> Fallible<()> {
        debug!("Creating API Server RBAC rule for kubelet");
        let yml_file = dir.join("rbac.yml");
        fs::write(&yml_file, include_str!("assets/apiserver.yml"))?;

        let output = Command::new("kubectl")
            .arg("apply")
            .arg(format!("--kubeconfig={}", admin_config.display()))
            .arg("-f")
            .arg(yml_file)
            .output()?;
        if !output.status.success() {
            debug!(
                "kubectl apply stdout: {}",
                String::from_utf8(output.stdout)?
            );
            debug!(
                "kubectl apply stderr: {}",
                String::from_utf8(output.stderr)?
            );
            bail!("kubectl apply command failed");
        }

        debug!("API Server RBAC rule created");
        Ok(())
    }
}

impl Stoppable for APIServer {
    fn stop(&mut self) -> Fallible<()> {
        self.process.stop()
    }
}
