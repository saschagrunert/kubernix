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
    process::{Process, ProcessState, stoppable},
    write_if_changed,
};
use anyhow::Context;
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
        create_dir_all(&dir).context("Unable to create proxy directory")?;

        let yml = Self::render(
            &kubeconfig.proxy().display().to_string(),
            &network.cluster_cidr().to_string(),
            config.is_rootless(),
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

    /// Render the proxy configuration. In rootless mode:
    ///
    /// - The conntrack sysctls are not writable from within a user
    ///   namespace, so a zero value tells kube-proxy to leave them untouched.
    /// - The br_netfilter module may be missing, because it cannot be loaded
    ///   from the user namespace. Replies between pods on the same bridge
    ///   then bypass iptables, so all Service traffic gets masqueraded to
    ///   route the replies through the gateway.
    fn render(kubeconfig: &str, cluster_cidr: &str, rootless: bool) -> String {
        let rootless = if rootless {
            concat!(
                "masqueradeAll: true\n",
                "conntrack:\n",
                "  maxPerCore: 0\n",
                "  tcpEstablishedTimeout: 0s\n",
                "  tcpCloseWaitTimeout: 0s\n",
            )
        } else {
            ""
        };
        format!(
            include_str!("assets/proxy.yml"),
            kubeconfig = kubeconfig,
            cluster_cidr = cluster_cidr,
            rootless = rootless,
        )
    }
}

stoppable!(Proxy);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_template_renders() {
        let yml = Proxy::render("/tmp/proxy.kubeconfig", "10.10.0.0/18", false);
        assert!(yml.contains("kind: KubeProxyConfiguration"));
        assert!(yml.contains("kubeconfig: \"/tmp/proxy.kubeconfig\""));
        assert!(yml.contains("clusterCIDR: \"10.10.0.0/18\""));
        assert!(yml.contains("mode: \"iptables\""));
        assert!(!yml.contains("conntrack:"));
        assert!(!yml.contains("masqueradeAll:"));
    }

    #[test]
    fn rootless_config_skips_conntrack_sysctls() {
        let yml = Proxy::render("/tmp/proxy.kubeconfig", "10.10.0.0/18", true);
        assert!(yml.contains("masqueradeAll: true\n"));
        assert!(yml.contains("conntrack:\n  maxPerCore: 0\n"));
        assert!(yml.contains("tcpEstablishedTimeout: 0s"));
        assert!(yml.contains("tcpCloseWaitTimeout: 0s"));
    }

    #[test]
    fn component_metadata() {
        let c = ProxyComponent;
        assert_eq!(c.name(), "Proxy");
        assert_eq!(c.phase(), Phase::Controller);
    }
}
