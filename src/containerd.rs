//! containerd container runtime component.
//!
//! Manages containerd instances that provide the container runtime
//! interface for kubelet. In multi-node mode, each containerd instance
//! runs inside its own container with an isolated CNI network
//! configuration.

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
pub struct ContainerdComponent {
    node: u8,
    name: String,
}

impl ContainerdComponent {
    /// Create a new containerd component for the given node index.
    pub fn new(node: u8) -> Self {
        Self {
            node,
            name: format!("Containerd (node {})", node),
        }
    }
}

impl Component for ContainerdComponent {
    fn name(&self) -> &str {
        &self.name
    }

    fn phase(&self) -> Phase {
        Phase::Controller
    }

    fn start(&self, ctx: &ClusterContext<'_>) -> ProcessState {
        Containerd::start(ctx.config, self.node, ctx.network)
    }
}

/// Manages a containerd process and its associated socket for a single node.
pub struct Containerd {
    process: Process,
    socket: CriSocket,
    node_name: String,
}

const CONTAINERD: &str = "containerd";

impl Containerd {
    /// Start a containerd instance for the given node index.
    pub fn start(config: &Config, node: u8, network: &Network) -> ProcessState {
        let node_name = Node::name(config, network, node);

        let crun_path: String;
        let plugin_dir: String;
        if config.multi_node() && !config.is_rootless() {
            // Use bare binary name so the runc v2 shim resolves crun from $PATH
            // inside the container (nix-shell provides it).
            crun_path = "crun".to_string();
            // Placeholder: patched at container startup time via sed.
            plugin_dir = "/tmp/cni-plugins".to_string();
        } else {
            crun_path = System::find_executable("crun")?.display().to_string();
            let loopback = System::find_executable("loopback")?;
            plugin_dir = loopback
                .parent()
                .context("Unable to find CNI plugin dir")?
                .display()
                .to_string();
        };

        let dir = Self::path(config, network, node);
        let config_file = dir.join("config.toml");
        let cni_conf_dir = dir.join("cni");
        let socket = Self::socket(config, network, node)?;

        // In rootless mode, kubelet sends oomScoreAdj via CRI, containerd
        // passes it to the OCI spec, and crun fails writing
        // /proc/self/oom_score_adj in a user namespace. Wrap crun to patch
        // config.json before exec (strip oomScoreAdj, fix namespace config).
        let runtime_path = if config.is_rootless() {
            dir.join("crun-rootless").display().to_string()
        } else {
            crun_path.clone()
        };

        if !dir.exists() {
            create_dir_all(&dir)?;
            create_dir_all(&cni_conf_dir)?;

            if config.is_rootless() {
                let wrapper = dir.join("crun-rootless");
                let script =
                    include_str!("assets/crun-rootless.sh").replace("__CRUN_PATH__", &crun_path);
                fs::write(&wrapper, script)?;
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    fs::set_permissions(&wrapper, fs::Permissions::from_mode(0o755))?;
                }
            }

            // containerd 2.3.3 native snapshotter fails with "no unpack
            // platforms defined"; overlayfs works in all modes.
            let snapshotter = "overlayfs";
            fs::write(
                &config_file,
                format!(
                    include_str!("assets/containerd.toml"),
                    root = dir.join("root").display(),
                    state = dir.join("state").display(),
                    socket = socket,
                    plugin_dir = plugin_dir,
                    cni_conf_dir = cni_conf_dir.display(),
                    runtime_path = runtime_path,
                    snapshotter = snapshotter,
                    disable_apparmor = config.is_rootless(),
                    disable_nri = config.is_rootless(),
                ),
            )?;

            cri::write_pod_network_config(config, &cni_conf_dir, &node_name, node, network)?;
        }

        // containerd-shim-runc-v2 hardcodes its socket path to /run/containerd/s/.
        // rootlesskit's --copy-up=/run creates an overlay that preserves
        // host-owned subdirectories with wrong ownership (host root maps to
        // UID 65534). Recreate with correct ownership. Safe: runs inside
        // rootlesskit's private mount namespace before containerd starts.
        if config.is_rootless() && create_dir_all("/run/containerd/s").is_err() {
            fs::remove_dir_all("/run/containerd")
                .context("Failed to remove host-owned /run/containerd")?;
            create_dir_all("/run/containerd/s")?;
        }

        let config_arg = format!("--config={}", config_file.display());
        let args: &[&str] = &[&config_arg];

        let mut process = if config.multi_node() && !config.is_rootless() {
            // containerd has no --cni-plugin-dir CLI flag (unlike CRI-O), so
            // patch bin_dirs in the config before starting the daemon. The
            // container entrypoint runs all args through `bash -c "$*"`, so
            // shell constructs (&&, $(...)) are interpreted.
            let identifier = format!("Containerd {}", node_name);
            let patch_bin_dirs = format!(
                r#"sed -i "s|bin_dirs = .*|bin_dirs = [\"$(dirname $(which loopback))\"]|" {} &&"#,
                config_file.display(),
            );
            let container_args: &[&str] = &[CONTAINERD, &config_arg];
            Container::start(
                config,
                &dir,
                &identifier,
                &patch_bin_dirs,
                &node_name,
                container_args,
            )?
        } else {
            Process::start(&dir, "Containerd", CONTAINERD, args)?
        };
        process.wait_ready("containerd successfully booted")?;

        Ok(Box::new(Self {
            process,
            socket,
            node_name,
        }))
    }

    /// Retrieve the CRI socket for the given node.
    pub fn socket(config: &Config, network: &Network, node: u8) -> Result<CriSocket> {
        CriSocket::new(Self::path(config, network, node).join("containerd.sock"))
    }

    /// Retrieve the working path for the node.
    fn path(config: &Config, network: &Network, node: u8) -> PathBuf {
        config
            .root()
            .join(CONTAINERD)
            .join(Node::name(config, network, node))
    }
}

impl Stoppable for Containerd {
    fn stop(&mut self) -> Result<()> {
        cri::remove_all_containers("containerd", &self.socket, &self.node_name).with_context(
            || {
                format!(
                    "Unable to remove containerd containers on {}",
                    self.node_name,
                )
            },
        )?;

        self.process.stop()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn component_metadata() {
        let c = ContainerdComponent::new(0);
        assert_eq!(c.name(), "Containerd (node 0)");
        assert_eq!(c.phase(), Phase::Controller);
    }

    #[test]
    fn component_name_per_node() {
        assert_eq!(ContainerdComponent::new(0).name(), "Containerd (node 0)");
        assert_eq!(ContainerdComponent::new(2).name(), "Containerd (node 2)");
    }
}
