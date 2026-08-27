//! CRI-O container runtime component.
//!
//! Manages CRI-O instances that provide the container runtime interface
//! for kubelet. In multi-node mode, each CRI-O instance runs inside its
//! own container with an isolated CNI network configuration.

use crate::{
    Config,
    component::{ClusterContext, Component, Phase},
    container::Container,
    cri::{self, CriSocket, RuntimePaths},
    network::Network,
    node::Node,
    process::{Process, ProcessState, Stoppable},
    system::System,
};
use anyhow::{Context, Result};
use std::{
    fs::{self, create_dir_all},
    path::PathBuf,
};

/// Component wrapper for registry-based startup (per-node).
pub struct CrioComponent {
    node: u8,
    name: String,
}

impl CrioComponent {
    /// Create a new CRI-O component for the given node index.
    pub fn new(node: u8) -> Self {
        Self {
            node,
            name: format!("CRI-O (node {})", node),
        }
    }
}

impl Component for CrioComponent {
    fn name(&self) -> &str {
        &self.name
    }

    fn phase(&self) -> Phase {
        // CRI-O only needs etcd/apiserver to be up, not the controllers,
        // so it starts in the Controller phase alongside scheduler/CM.
        Phase::Controller
    }

    fn start(&self, ctx: &ClusterContext<'_>) -> ProcessState {
        Crio::start(ctx.config, self.node, ctx.network)
    }
}

/// Manages a CRI-O process and its associated socket for a single node.
pub struct Crio {
    process: Process,
    socket: CriSocket,
    node_name: String,
}

const CRIO: &str = "crio";

impl Crio {
    /// Start a CRI-O instance for the given node index.
    pub fn start(config: &Config, node: u8, network: &Network) -> ProcessState {
        let node_name = Node::name(config, network, node);

        let paths = RuntimePaths::resolve(config)?;
        // CRI-O validates runtime_path with stat; empty lets it resolve from $PATH
        let crun_path = if config.multi_node() && !config.is_rootless() {
            String::new()
        } else {
            paths.crun
        };
        let plugin_dir = paths.plugin_dir;
        let conmon = if config.multi_node() && !config.is_rootless() {
            String::new()
        } else {
            System::find_executable("conmon")?.display().to_string()
        };

        let dir = Self::path(config, network, node);
        let config_dir = dir.join("crio.conf.d");
        let config_file = config_dir.join("crio.conf");
        let network_dir = dir.join("cni");
        let socket = Self::socket(config, network, node)?;

        if !dir.exists() {
            create_dir_all(&dir).context("Unable to create CRI-O directory")?;
            create_dir_all(&network_dir).context("Unable to create CRI-O CNI directory")?;
            create_dir_all(&config_dir).context("Unable to create CRI-O config directory")?;

            let attach_dir = dir.join("attach");
            let ns_dir = dir.join("ns");
            create_dir_all(&attach_dir).context("Unable to create CRI-O attach directory")?;
            create_dir_all(&ns_dir).context("Unable to create CRI-O namespace directory")?;

            let containers_dir = dir.join("containers");
            fs::write(
                &config_file,
                format!(
                    include_str!("assets/crio.conf"),
                    attach_socket_dir = attach_dir.display(),
                    conmon = conmon,
                    containers_root = containers_dir.join("storage").display(),
                    containers_runroot = containers_dir.join("run").display(),
                    listen = socket,
                    log_dir = dir.join("log").display(),
                    namespaces_dir = ns_dir.display(),
                    network_dir = network_dir.display(),
                    plugin_dir = plugin_dir,
                    exits_dir = dir.join("exits").display(),
                    runtime_path = crun_path,
                    runtime_root = dir.join("crun").display(),
                    signature_policy = Container::policy_json(config).display(),
                    storage_driver = "overlay",
                    storage_option = "",
                    version_file = dir.join("version").display(),
                    disable_hostport_mapping = config.is_rootless(),
                    enable_nri = !config.is_rootless(),
                ),
            )
            .context("Unable to write CRI-O config")?;

            cri::write_pod_network_config(config, &network_dir, &node_name, node, network)?;
        }
        let config_dir_arg = format!("--config-dir={}", config_dir.display());
        let args: &[&str] = &[&config_dir_arg];

        let mut process = if config.multi_node() && !config.is_rootless() {
            // Run inside a container, resolve CNI plugin dir from $PATH at runtime
            let identifier = format!("CRI-O {}", node_name);
            let plugin_dir_arg =
                r#"--cni-plugin-dir=$(dirname $(which loopback || echo loopback_not_found))"#
                    .to_string();
            let container_args: &[&str] = &[&config_dir_arg, &plugin_dir_arg];
            Container::start(config, &dir, &identifier, CRIO, &node_name, container_args)?
        } else {
            Process::start(&dir, "CRI-O", CRIO, args)?
        };
        process.wait_ready("No systemd watchdog enabled")?;

        Ok(Box::new(Self {
            process,
            socket,
            node_name,
        }))
    }

    /// Retrieve the CRI socket
    pub fn socket(config: &Config, network: &Network, node: u8) -> Result<CriSocket> {
        CriSocket::new(Self::path(config, network, node).join("crio.sock"))
    }

    /// Retrieve the working path for the node
    fn path(config: &Config, network: &Network, node: u8) -> PathBuf {
        config
            .root()
            .join(CRIO)
            .join(Node::name(config, network, node))
    }
}

impl Stoppable for Crio {
    fn stop(&mut self) -> Result<()> {
        cri::remove_all_containers("CRI-O", &self.socket, &self.node_name)
            .with_context(|| format!("Unable to remove CRI-O containers on {}", self.node_name))?;

        self.process.stop()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn component_metadata() {
        let c = CrioComponent::new(0);
        assert_eq!(c.name(), "CRI-O (node 0)");
        assert_eq!(c.phase(), Phase::Controller);
    }

    #[test]
    fn component_name_per_node() {
        assert_eq!(CrioComponent::new(0).name(), "CRI-O (node 0)");
        assert_eq!(CrioComponent::new(2).name(), "CRI-O (node 2)");
    }
}
