//! etcd distributed key-value store component.
//!
//! Provides the backing store for all Kubernetes cluster data.
//! Starts a single-node etcd instance with TLS mutual authentication.

use crate::{
    component::{ClusterContext, Component, Phase},
    config::Config,
    network::Network,
    pki::Pki,
    process::{Process, ProcessState, Stoppable},
};
use anyhow::Result;
use std::fs::create_dir_all;

/// Component wrapper for registry-based startup.
pub struct EtcdComponent;

impl Component for EtcdComponent {
    fn name(&self) -> &str {
        "etcd"
    }

    fn phase(&self) -> Phase {
        Phase::Infrastructure
    }

    fn start(&self, ctx: &ClusterContext<'_>) -> ProcessState {
        Etcd::start(ctx.config, ctx.network, ctx.pki)
    }
}

/// Manages the etcd process lifecycle.
pub struct Etcd {
    process: Process,
}

impl Etcd {
    /// Start etcd with TLS and the given cluster configuration.
    pub fn start(config: &Config, network: &Network, pki: &Pki) -> ProcessState {
        const ETCD: &str = "etcd";
        let dir = config.root().join(ETCD);
        create_dir_all(&dir)?;

        let mut process = Process::start(
            &dir,
            ETCD,
            ETCD,
            &[
                "--auto-compaction-mode=periodic",
                "--auto-compaction-retention=1h",
                "--client-cert-auth",
                "--initial-cluster-state=new",
                "--initial-cluster-token=etcd-cluster",
                "--peer-client-cert-auth",
                "--snapshot-count=5000",
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
    fn component_metadata() {
        let c = EtcdComponent;
        assert_eq!(c.name(), "etcd");
        assert_eq!(c.phase(), Phase::Infrastructure);
    }

    /// Integration test: requires etcd and cfssl binaries in $PATH.
    /// Run with: cargo test -- --ignored
    #[test]
    #[ignore]
    fn start_and_stop() -> Result<()> {
        let c = test_config()?;
        let n = test_network()?;
        let p = Pki::new(&c, &n)?;

        let mut etcd = Etcd::start(&c, &n, &p)?;
        etcd.stop()
    }
}
