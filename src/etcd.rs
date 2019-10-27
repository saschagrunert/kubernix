use crate::{
    config::Config,
    network::Network,
    pki::Pki,
    process::{Process, ProcessState, Stoppable},
};
use anyhow::Result;
use std::fs::create_dir_all;

pub struct Etcd {
    process: Process,
}

impl Etcd {
    pub fn start(config: &Config, network: &Network, pki: &Pki) -> ProcessState {
        const ETCD: &str = "etcd";
        let dir = config.root().join(ETCD);
        create_dir_all(&dir)?;

        let mut process = Process::start(
            &dir,
            ETCD,
            ETCD,
            &[
                "--client-cert-auth",
                "--initial-cluster-state=new",
                "--initial-cluster-token=etcd-cluster",
                "--peer-client-cert-auth",
                &format!(
                    "--initial-advertise-peer-urls=https://{}",
                    network.etcd_peer()
                ),
                &format!("--advertise-client-urls=https://{}", network.etcd_client()),
                &format!("--cert-file={}", pki.apiserver().cert().display()),
                &format!("--data-dir={}", dir.join("run").display()),
                &format!("--initial-cluster=etcd=https://{}", network.etcd_peer()),
                &format!("--key-file={}", pki.apiserver().key().display()),
                &format!("--listen-client-urls=https://{}", network.etcd_client()),
                &format!("--listen-peer-urls=https://{}", network.etcd_peer()),
                &format!("--name={}", ETCD),
                &format!("--peer-cert-file={}", pki.apiserver().cert().display()),
                &format!("--peer-key-file={}", pki.apiserver().key().display()),
                &format!("--peer-trusted-ca-file={}", pki.ca().cert().display()),
                &format!("--trusted-ca-file={}", pki.ca().cert().display()),
            ],
        )?;

        process.wait_ready("ready to serve client requests")?;
        Ok(Box::new(Self { process }))
    }
}

impl Stoppable for Etcd {
    fn stop(&mut self) -> Result<()> {
        self.process.stop()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{config::tests::test_config, network::tests::test_network};

    #[test]
    fn new_success() -> Result<()> {
        let c = test_config()?;
        let n = test_network()?;
        let p = Pki::new(&c, &n)?;

        let mut etcd = Etcd::start(&c, &n, &p)?;
        etcd.stop()
    }
}
