//! Kubernetes network proxy component.
//!
//! Runs `kube-proxy` which maintains network rules on nodes, enabling
//! Kubernetes Service abstraction by forwarding traffic to backend pods.

use crate::{
    component::{ClusterContext, Component, Phase},
    config::Config,
    kubeconfig::KubeConfig,
    network::Network,
    node::Node,
    process::{Process, ProcessState, Stoppable},
    write_if_changed,
};
use anyhow::Result;
use std::fs::create_dir_all;

/// Component wrapper for registry-based startup.
pub struct ProxyComponent;

impl Component for ProxyComponent {
    fn name(&self) -> &str {
        "Proxy"
    }

    fn phase(&self) -> Phase {
        // Proxy only needs the API server to sync caches, not the
        // kubelets, so it starts in the Controller phase.
        Phase::Controller
    }

    fn start(&self, ctx: &ClusterContext<'_>) -> ProcessState {
        Proxy::start(ctx.config, ctx.network, ctx.kubeconfig)
    }
}

/// Manages the `kube-proxy` process lifecycle.
pub struct Proxy {
    process: Process,
}

impl Proxy {
    /// Start the proxy with the given cluster configuration.
    pub fn start(config: &Config, network: &Network, kubeconfig: &KubeConfig) -> ProcessState {
        let dir = config.root().join("proxy");
        create_dir_all(&dir)?;

        let yml = format!(
            include_str!("assets/proxy.yml"),
            kubeconfig.proxy().display(),
            network.cluster_cidr(),
        );
        let cfg = dir.join("config.yml");
        write_if_changed(&cfg, &yml)?;

        let mut process = Process::start(
            &dir,
            "Proxy",
            "kube-proxy",
            &[
                &format!("--config={}", cfg.display()),
                &format!(
                    "--hostname-override={}",
                    if config.multi_node() {
                        Node::name(config, network, 0)
                    } else {
                        network.hostname().into()
                    }
                ),
            ],
        )?;

        process.wait_ready("Caches are synced")?;
        Ok(Box::new(Proxy { process }))
    }
}

impl Stoppable for Proxy {
    fn stop(&mut self) -> Result<()> {
        self.process.stop()
    }
}
