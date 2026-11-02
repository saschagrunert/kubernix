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
    process::{self, Process, ProcessState, stoppable},
    write_if_changed,
};
use anyhow::Context;
use log::{debug, warn};
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

    /// Remove the rules kube-proxy leaves behind after it stopped, like the
    /// `KUBE-*` iptables chains. In rootless mode, they belong to the
    /// network namespace of rootlesskit, which gets removed anyway.
    pub fn cleanup_rules() {
        debug!("Removing the kube-proxy rules");
        match process::command("kube-proxy").arg("--cleanup").output() {
            Ok(o) if o.status.success() => {}
            // It also fails if a proxy mode is not supported by the host,
            // like IPVS, after removing the rules of the other modes.
            Ok(o) => debug!(
                "kube-proxy --cleanup exited with {}: {}",
                o.status,
                String::from_utf8_lossy(&o.stderr).trim()
            ),
            Err(e) => warn!(
                "Unable to run 'kube-proxy --cleanup', the kube-proxy rules may need \
                 manual cleanup: {}",
                e
            ),
        }
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
                "iptables:\n",
                "  masqueradeAll: true\n",
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
        let yml = Proxy::render("/tmp/proxy.kubeconfig", "10.10.0.0/17", false);
        assert!(yml.contains("kind: KubeProxyConfiguration"));
        assert!(yml.contains("kubeconfig: \"/tmp/proxy.kubeconfig\""));
        assert!(yml.contains("clusterCIDR: \"10.10.0.0/17\""));
        assert!(yml.contains("mode: \"iptables\""));
        assert!(!yml.contains("conntrack:"));
        assert!(!yml.contains("masqueradeAll:"));
    }

    #[test]
    fn rootless_config_skips_conntrack_sysctls() {
        let yml = Proxy::render("/tmp/proxy.kubeconfig", "10.10.0.0/17", true);
        // masqueradeAll is no top level field of the v1alpha1 config
        assert!(yml.contains("\niptables:\n  masqueradeAll: true\n"));
        assert!(!yml.lines().any(|l| l.starts_with("masqueradeAll")));
        assert!(yml.contains("conntrack:\n  maxPerCore: 0\n"));
        assert!(yml.contains("tcpEstablishedTimeout: 0s"));
        assert!(yml.contains("tcpCloseWaitTimeout: 0s"));
    }
}
