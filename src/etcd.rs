use crate::{config::Config, pki::Pki, process::Process};
use failure::Fallible;
use log::debug;

pub struct Etcd {
    process: Process,
}

impl Etcd {
    pub fn new(config: &Config, pki: &Pki) -> Fallible<Etcd> {
        debug!("Starting etcd");
        let mut process = Process::new(
            config,
            &[
                "etcd",
                "--advertise-client-urls=https://127.0.0.1:2379",
                "--client-cert-auth",
                "--data-dir=etcd",
                "--initial-advertise-peer-urls=https://127.0.0.1:2380",
                "--initial-cluster-state=new",
                "--initial-cluster-token=etcd-cluster",
                "--initial-cluster=etcd=https://127.0.0.1:2380",
                "--listen-client-urls=https://127.0.0.1:2379",
                "--listen-peer-urls=https://127.0.0.1:2380",
                "--name=etcd",
                "--peer-client-cert-auth",
                &format!("--cert-file={}", pki.apiserver_cert.display()),
                &format!("--key-file={}", pki.apiserver_key.display()),
                &format!("--peer-cert-file={}", pki.apiserver_cert.display()),
                &format!("--peer-key-file={}", pki.apiserver_key.display()),
                &format!("--peer-trusted-ca-file={}", pki.ca.display()),
                &format!("--trusted-ca-file={}", pki.ca.display()),
            ],
        )?;

        process.wait_ready("ready to serve client requests")?;
        debug!("etcd is ready");
        Ok(Etcd { process })
    }

    pub fn stop(&mut self) -> Fallible<()> {
        self.process.stop()?;
        Ok(())
    }
}
