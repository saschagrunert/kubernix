//! CRI-O container runtime component.
//!
//! Manages CRI-O instances that provide the container runtime interface
//! for kubelet. In multi-node mode, each CRI-O instance runs inside its
//! own container with an isolated CNI network configuration.

use crate::{
    Config,
    component::{ClusterContext, Component, Phase},
    container::Container,
    cri::{self, CriSocket},
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

        // In multi-node mode, CRI-O runs inside a container where host paths
        // don't exist. Use empty paths so CRI-O resolves binaries from $PATH
        // (set up by nix-shell in the container).
        let conmon: String;
        let crun_path: String;
        let plugin_dir: String;
        if config.multi_node() {
            // Empty paths let CRI-O resolve conmon/crun from $PATH inside the container.
            conmon = String::new();
            crun_path = String::new();
            // Placeholder: overridden by --cni-plugin-dir CLI arg at startup.
            plugin_dir = "/tmp/cni-plugins".to_string();
        } else {
            conmon = System::find_executable("conmon")?.display().to_string();
            crun_path = System::find_executable("crun")?.display().to_string();
            let loopback = System::find_executable("loopback")?;
            plugin_dir = loopback
                .parent()
                .context("Unable to find CNI plugin dir")?
                .display()
                .to_string();
        };

        let dir = Self::path(config, network, node);
        let config_dir = dir.join("crio.conf.d");
        let config_file = config_dir.join("crio.conf");
        let network_dir = dir.join("cni");
        let socket = Self::socket(config, network, node)?;

        if !dir.exists() {
            create_dir_all(&dir)?;
            create_dir_all(&network_dir)?;
            create_dir_all(&config_dir)?;

            let containers_dir = dir.join("containers");
            fs::write(
                &config_file,
                format!(
                    include_str!("assets/crio.conf"),
                    conmon = conmon,
                    containers_root = containers_dir.join("storage").display(),
                    containers_runroot = containers_dir.join("run").display(),
                    listen = socket,
                    log_dir = dir.join("log").display(),
                    network_dir = network_dir.display(),
                    plugin_dir = plugin_dir,
                    exits_dir = dir.join("exits").display(),
                    runtime_path = crun_path,
                    runtime_root = dir.join("crun").display(),
                    signature_policy = Container::policy_json(config).display(),
                    storage_driver = if config.multi_node() || System::in_container()? {
                        "vfs"
                    } else {
                        "overlay"
                    },
                    version_file = dir.join("version").display(),
                ),
            )?;

            cri::write_cni_config(&network_dir, &node_name, node, network)?;
        }
        let config_dir_arg = format!("--config-dir={}", config_dir.display());
        let args: &[&str] = &[&config_dir_arg];

        let mut process = if config.multi_node() {
            // Run inside a container, resolve CNI plugin dir from $PATH at runtime
            let identifier = format!("CRI-O {}", node_name);
            let plugin_dir_arg =
                r#"--cni-plugin-dir=$(dirname $(which loopback || echo loopback_not_found))"#
                    .to_string();
            let container_args: &[&str] = &[&config_dir_arg, &plugin_dir_arg];
            Container::start(config, &dir, &identifier, CRIO, &node_name, container_args)?
        } else {
            // Run as usual process
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
