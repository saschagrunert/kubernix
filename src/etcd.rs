//! etcd distributed key-value store component.
//!
//! Provides the backing store for all Kubernetes cluster data.
//! Starts a single-node etcd instance with TLS mutual authentication.

use crate::{
    component::{ClusterContext, Component, Phase},
    config::Config,
    network::Network,
    pki::Pki,
    process::{Process, ProcessState, stoppable},
};
use anyhow::Context;
use std::{fs::create_dir_all, path::PathBuf};

const ETCD: &str = "etcd";

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
    /// The data directory of etcd.
    pub fn data_dir(config: &Config) -> PathBuf {
        config.root().join(ETCD).join("run")
    }

    /// Returns true if etcd stored data, which depends on the cluster CA,
    /// the ServiceAccount key and the encryption key.
    pub fn has_data(config: &Config) -> bool {
        Self::data_dir(config).join("member").exists()
    }

    /// Start etcd with TLS and the given cluster configuration.
    pub fn start(config: &Config, network: &Network, pki: &Pki) -> ProcessState {
        let dir = config.root().join(ETCD);
        create_dir_all(&dir).context("Unable to create etcd directory")?;

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
                &format!("--cert-file={}", pki.etcd().cert().display()),
                &format!("--data-dir={}", Self::data_dir(config).display()),
                &format!("--initial-cluster=etcd=https://{}", network.etcd_peer()),
                &format!("--key-file={}", pki.etcd().key().display()),
                &format!("--listen-client-urls=https://{}", network.etcd_client()),
                &format!("--listen-peer-urls=https://{}", network.etcd_peer()),
                &format!("--name={}", ETCD),
                &format!("--peer-cert-file={}", pki.etcd_peer().cert().display()),
                &format!("--peer-key-file={}", pki.etcd_peer().key().display()),
                &format!("--peer-trusted-ca-file={}", pki.etcd_ca().cert().display()),
                &format!("--trusted-ca-file={}", pki.etcd_ca().cert().display()),
            ],
        )?;

        process.wait_ready("ready to serve client requests")?;
        Ok(Box::new(Self { process }))
    }
}

stoppable!(Etcd);
