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

pub struct ApiServer {
    process: Process,
}

impl ApiServer {
    pub fn start(
        config: &Config,
        ip: &str,
        pki: &Pki,
        encryptionconfig: &EncryptionConfig,
        kubeconfig: &KubeConfig,
    ) -> Fallible<Startable> {
        info!("Starting API Server");

        let dir = config.root().join("apiserver");
        create_dir_all(&dir)?;

        let mut process = Process::start(
            config,
            &dir,
            "kube-apiserver",
            &[
                &format!("--advertise-address={}", ip),
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
                &format!(
                    "--etcd-servers=https://{}:2379",
                    Ipv4Addr::LOCALHOST.to_string(),
                ),
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
                "--kubelet-https=true",
                "--runtime-config=api/all",
                &format!(
                    "--service-account-key-file={}",
                    pki.service_account().cert().display()
                ),
                &format!("--service-cluster-ip-range={}", config.service_cidr()),
                &format!("--tls-cert-file={}", pki.apiserver().cert().display()),
                &format!("--tls-private-key-file={}", pki.apiserver().key().display()),
                "--v=2",
            ],
        )?;

        process.wait_ready("etcd ok")?;
        Self::setup_rbac(&dir, kubeconfig.admin())?;
        info!("API Server is ready");
        Ok(Box::new(ApiServer { process }))
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

impl Stoppable for ApiServer {
    fn stop(&mut self) -> Fallible<()> {
        self.process.stop()
    }
}
