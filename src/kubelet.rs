//! Kubernetes kubelet node agent component.
//!
//! Runs `kubelet` on each node, responsible for managing pods and
//! containers via the configured CRI runtime.

use crate::{
    component::{ClusterContext, Component, Phase},
    config::Config,
    container::Container,
    cri::{self, MAX_SOCKET_PATH_LEN},
    kubeconfig::KubeConfig,
    network::Network,
    node::Node,
    pki::Pki,
    process::{Process, ProcessState, stoppable},
    system::System,
    write_if_changed,
};
use anyhow::{Context, Result, bail};
use std::{fs::create_dir_all, path::Path};

const KUBELET: &str = "kubelet";
const KUBELET_PORT_BASE: u16 = 11250;
const HEALTHZ_PORT_BASE: u16 = 12250;

/// Component wrapper for registry-based startup (per-node).
pub struct KubeletComponent {
    node: u8,
    name: String,
}

impl KubeletComponent {
    /// Create a new Kubelet component for the given node index.
    pub fn new(node: u8) -> Self {
        Self {
            node,
            name: format!("Kubelet (node {})", node),
        }
    }
}

impl Component for KubeletComponent {
    fn name(&self) -> &str {
        &self.name
    }

    fn phase(&self) -> Phase {
        Phase::NodeAgent
    }

    fn start(&self, ctx: &ClusterContext<'_>) -> ProcessState {
        Kubelet::start(ctx.config, self.node, ctx.network, ctx.pki, ctx.kubeconfig)
    }
}

/// Manages the kubelet process lifecycle for a single node.
pub struct Kubelet {
    process: Process,
}

impl Kubelet {
    /// Ensure that the unix sockets of the node fit into the length limit.
    pub fn validate_paths(config: &Config, network: &Network, node: u8) -> Result<()> {
        let root_dir = config
            .root()
            .join(KUBELET)
            .join(Node::name(config, network, node))
            .join("run");
        // pod-resources/<pid> is the longest socket path kubelet creates
        if root_dir.as_os_str().len() + "pod-resources/1234567890".len() > MAX_SOCKET_PATH_LEN {
            bail!(
                "Kubelet run path '{}' is too long for unix sockets, use a shorter --root",
                root_dir.display()
            )
        }
        cri::cri_socket(config, network, node)?;
        Ok(())
    }

    /// Start the kubelet for the given node index.
    pub fn start(
        config: &Config,
        node: u8,
        network: &Network,
        pki: &Pki,
        kubeconfig: &KubeConfig,
    ) -> ProcessState {
        let node_name = Node::name(config, network, node);

        let dir = config.root().join(KUBELET).join(&node_name);
        let root_dir = dir.join("run");

        create_dir_all(&dir).context("Unable to create kubelet directory")?;

        let identity = pki
            .kubelets()
            .get(node as usize)
            .with_context(|| format!("Unable to retrieve kubelet identity for {}", node_name))?;

        let ca = pki.ca().cert().display().to_string();
        let dns = network.dns()?;
        let cidr = network
            .pod_cidrs()
            .get(node as usize)
            .context("Unable to retrieve kubelet CIDR")?;
        let cert = identity.cert().display().to_string();
        let key = identity.key().display().to_string();
        let port = KUBELET_PORT_BASE + u16::from(node);
        let healthz_port = HEALTHZ_PORT_BASE + u16::from(node);
        let cri_socket = cri::cri_socket(config, network, node)?.to_socket_string();
        let yml = if config.is_rootless() {
            format!(
                include_str!("assets/kubelet-rootless.yml"),
                ca = ca,
                dns = dns,
                cidr = cidr,
                cert = cert,
                key = key,
                cri_socket = cri_socket,
                pod_logs_dir = dir.join("pods").display(),
                port = port,
                healthzPort = healthz_port,
            )
        } else {
            format!(
                include_str!("assets/kubelet.yml"),
                ca = ca,
                dns = dns,
                cidr = cidr,
                cert = cert,
                key = key,
                cri_socket = cri_socket,
                port = port,
                healthzPort = healthz_port,
            )
        };
        let cfg = dir.join("config.yml");
        write_if_changed(&cfg, &yml)?;

        let args = &[
            &format!("--config={}", cfg.display()),
            &format!("--root-dir={}", root_dir.display()),
            &format!(
                "--kubeconfig={}",
                kubeconfig
                    .kubelets()
                    .get(node as usize)
                    .with_context(|| format!(
                        "Unable to retrieve kubelet config for {}",
                        node_name
                    ))?
                    .display()
            ),
            "--v=2",
        ];

        let mut process = if config.multi_node() && !config.is_rootless() {
            // Run inside a container
            let arg_hostname = &format!("--hostname-override={}", node_name);
            let mut modargs: Vec<&str> = vec![arg_hostname];
            modargs.extend(args);
            Container::exec(
                config,
                &dir,
                &format!("Kubelet {}", node_name),
                KUBELET,
                &node_name,
                &modargs,
            )?
        } else if config.multi_node() {
            // Rootless multi-node: direct process with hostname override and
            // its own device plugin directory
            let arg_hostname = format!("--hostname-override={}", node_name);
            let mut modargs: Vec<&str> = vec![&arg_hostname];
            modargs.extend(args);
            let device_plugins = dir.join("device-plugins");
            create_dir_all(&device_plugins)
                .context("Unable to create kubelet device plugin directory")?;
            Process::start_with_bind_mount(
                &dir,
                "Kubelet",
                KUBELET,
                &modargs,
                &device_plugins,
                Path::new(System::DEVICE_PLUGINS_DIR),
            )?
        } else {
            // Single-node: run as usual process
            Process::start(&dir, "Kubelet", KUBELET, args)?
        };
        process.wait_ready("Successfully registered node")?;
        Ok(Box::new(Self { process }))
    }
}

stoppable!(Kubelet);

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cri::MAX_SOCKET_PATH_LEN;
    use clap::Parser;

    #[test]
    fn config_template_renders() {
        let yml = format!(
            include_str!("assets/kubelet.yml"),
            ca = "/tmp/ca.pem",
            dns = "10.10.64.2",
            cidr = "10.10.128.0/18",
            cert = "/tmp/kubelet.pem",
            key = "/tmp/kubelet-key.pem",
            cri_socket = "unix:///run/crio/crio.sock",
            port = KUBELET_PORT_BASE,
            healthzPort = HEALTHZ_PORT_BASE,
        );
        assert!(yml.contains("kind: KubeletConfiguration"));
        assert!(yml.contains("clientCAFile: \"/tmp/ca.pem\""));
        assert!(yml.contains("clusterDNS:"));
        assert!(yml.contains("- \"10.10.64.2\""));
        assert!(yml.contains("podCIDR: \"10.10.128.0/18\""));
        assert!(yml.contains("containerRuntimeEndpoint: \"unix:///run/crio/crio.sock\""));
        assert!(yml.contains(&format!("port: {}", KUBELET_PORT_BASE)));
        assert!(yml.contains(&format!("healthzPort: {}", HEALTHZ_PORT_BASE)));
    }

    #[test]
    fn validate_paths_short_root() -> Result<()> {
        let config = crate::config::tests::test_config()?;
        let network = Network::new(&config)?;
        Kubelet::validate_paths(&config, &network, 0)
    }

    #[test]
    fn validate_paths_long_kubelet_root() -> Result<()> {
        // The CRI socket path fits, but the kubelet pod resources socket not
        let node = Network::new(&crate::config::tests::test_config()?)?
            .hostname()
            .to_string();
        let root = format!("/{}", "a".repeat(MAX_SOCKET_PATH_LEN - node.len() - 20));
        let config = Config::parse_from(["kubernix", "--root", &root]);
        let network = Network::new(&config)?;
        cri::cri_socket(&config, &network, 0)?;
        let err = Kubelet::validate_paths(&config, &network, 0).unwrap_err();
        assert!(err.to_string().contains("Kubelet run path"));
        Ok(())
    }

    #[test]
    fn component_metadata() {
        let c = KubeletComponent::new(0);
        assert_eq!(c.name(), "Kubelet (node 0)");
        assert_eq!(c.phase(), Phase::NodeAgent);
    }

    #[test]
    fn component_name_per_node() {
        assert_eq!(KubeletComponent::new(0).name(), "Kubelet (node 0)");
        assert_eq!(KubeletComponent::new(3).name(), "Kubelet (node 3)");
    }

    #[test]
    fn rootless_config_template_renders() {
        let yml = format!(
            include_str!("assets/kubelet-rootless.yml"),
            ca = "/tmp/ca.pem",
            dns = "10.10.64.2",
            cidr = "10.10.128.0/18",
            cert = "/tmp/kubelet.pem",
            key = "/tmp/kubelet-key.pem",
            cri_socket = "unix:///run/crio/crio.sock",
            pod_logs_dir = "/tmp/kubelet/node-0/pods",
            port = KUBELET_PORT_BASE,
            healthzPort = HEALTHZ_PORT_BASE,
        );
        assert!(yml.contains("kind: KubeletConfiguration"));
        // The kubelet takes the cgroup driver from the CRI runtime
        assert!(!yml.contains("cgroupDriver"));
        assert!(yml.contains("podLogsDir: \"/tmp/kubelet/node-0/pods\""));
        assert!(yml.contains("containerRuntimeEndpoint: \"unix:///run/crio/crio.sock\""));
        assert!(yml.contains("KubeletInUserNamespace: true"));
    }
}
