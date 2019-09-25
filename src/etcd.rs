use crate::{config::Config, pki::Pki, process::Process};
use failure::Fallible;
use log::info;

pub struct Etcd {
    process: Process,
}

impl Etcd {
    pub fn new(config: &Config, pki: &Pki) -> Fallible<Etcd> {
        info!("Starting etcd");
        let mut process = Process::new(
            config,
            &[
                "etcd".to_owned(),
                "--advertise-client-urls=https://127.0.0.1:2379".to_owned(),
                "--client-cert-auth".to_owned(),
                format!("--data-dir={}", config.root.join("etcd").display()),
                "--initial-advertise-peer-urls=https://127.0.0.1:2380"
                    .to_owned(),
                "--initial-cluster-state=new".to_owned(),
                "--initial-cluster-token=etcd-cluster".to_owned(),
                "--initial-cluster=etcd=https://127.0.0.1:2380".to_owned(),
                "--listen-client-urls=https://127.0.0.1:2379".to_owned(),
                "--listen-peer-urls=https://127.0.0.1:2380".to_owned(),
                "--name=etcd".to_owned(),
                "--peer-client-cert-auth".to_owned(),
                format!("--cert-file={}", pki.apiserver_cert.display()),
                format!("--key-file={}", pki.apiserver_key.display()),
                format!("--peer-cert-file={}", pki.apiserver_cert.display()),
                format!("--peer-key-file={}", pki.apiserver_key.display()),
                format!("--peer-trusted-ca-file={}", pki.ca.display()),
                format!("--trusted-ca-file={}", pki.ca.display()),
            ],
        )?;

        process.wait_ready("ready to serve client requests")?;
        info!("etcd is ready");
        Ok(Etcd { process })
    }

    pub fn stop(&mut self) -> Fallible<()> {
        self.process.stop()?;
        Ok(())
    }
}
