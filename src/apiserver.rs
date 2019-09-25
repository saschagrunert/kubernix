use crate::{
    config::Config, encryptionconfig::EncryptionConfig, pki::Pki,
    process::Process,
};
use failure::Fallible;
use log::info;
use std::fs::create_dir_all;

pub struct APIServer {
    process: Process,
}

impl APIServer {
    pub fn new(
        config: &Config,
        ip: &str,
        pki: &Pki,
        encryptionconfig: &EncryptionConfig,
    ) -> Fallible<APIServer> {
        info!("Starting API sever");

        let dir = config.root.join("api-server");
        create_dir_all(&dir)?;

        let mut process = Process::new(
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
                format!("--client-ca-file={}", pki.ca.display()),
                format!("--etcd-cafile={}", pki.ca.display()),
                format!("--etcd-certfile={}", pki.apiserver_cert.display()),
                format!("--etcd-keyfile={}", pki.apiserver_key.display()),
                "--etcd-servers=https://127.0.0.1:2379".to_owned(),
                "--event-ttl=1h".to_owned(),
                format!(
                    "--encryption-provider-config={}",
                    encryptionconfig.path.display()
                ),
                format!("--kubelet-certificate-authority={}", pki.ca.display()),
                format!(
                    "--kubelet-client-certificate={}",
                    pki.apiserver_cert.display()
                ),
                format!("--kubelet-client-key={}", pki.apiserver_key.display()),
                "--kubelet-https=true".to_owned(),
                "--runtime-config=api/all".to_owned(),
                format!(
                    "--service-account-key-file={}",
                    pki.service_account_cert.display()
                ),
                "--service-cluster-ip-range=10.32.0.0/24".to_owned(),
                format!("--tls-cert-file={}", pki.apiserver_cert.display()),
                format!(
                    "--tls-private-key-file={}",
                    pki.apiserver_key.display()
                ),
                "--v=2".to_owned(),
            ],
        )?;

        process.wait_ready("etcd ok")?;
        info!("API server is ready");
        Ok(APIServer { process })
    }

    pub fn stop(&mut self) -> Fallible<()> {
        self.process.stop()?;
        Ok(())
    }
}
