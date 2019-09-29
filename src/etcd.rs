use crate::{
    config::Config,
    pki::Pki,
    process::{Process, Startable, Stoppable},
    LOCALHOST,
};
use log::info;

pub struct Etcd {
    process: Process,
}

impl Etcd {
    pub fn start(config: &Config, pki: &Pki) -> Startable {
        info!("Starting etcd");

        let etcd_localhost = format!("https://{}:2379", LOCALHOST);
        let etcd_localhost_peer = format!("https://{}:2380", LOCALHOST);

        let mut process = Process::new(
            config,
            &[
                "etcd".to_owned(),
                format!("--advertise-client-urls={}", etcd_localhost),
                "--client-cert-auth".to_owned(),
                format!("--data-dir={}", config.root.join("etcd").display()),
                format!(
                    "--initial-advertise-peer-urls={}",
                    etcd_localhost_peer
                ),
                "--initial-cluster-state=new".to_owned(),
                "--initial-cluster-token=etcd-cluster".to_owned(),
                format!("--initial-cluster=etcd={}", etcd_localhost_peer),
                format!("--listen-client-urls={}", etcd_localhost),
                format!("--listen-peer-urls={}", etcd_localhost_peer),
                "--name=etcd".to_owned(),
                "--peer-client-cert-auth".to_owned(),
                format!("--cert-file={}", pki.apiserver.cert().display()),
                format!("--key-file={}", pki.apiserver.key().display()),
                format!("--peer-cert-file={}", pki.apiserver.cert().display()),
                format!("--peer-key-file={}", pki.apiserver.key().display()),
                format!("--peer-trusted-ca-file={}", pki.ca.cert().display()),
                format!("--trusted-ca-file={}", pki.ca.cert().display()),
            ],
        )?;

        process.wait_ready("ready to serve client requests")?;
        info!("etcd is ready");
        Ok(Box::new(Etcd { process }))
    }
}

impl Stoppable for Etcd {
    fn stop(&mut self) {
        self.process.stop();
    }
}
