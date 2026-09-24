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
    path::{Path, PathBuf},
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

            fs::write(
                &config_file,
                Self::render_config(config, &dir, &socket, &conmon, &paths),
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

    /// Render the CRI-O configuration for the node working directory.
    ///
    /// Both OCI runtimes are registered as runtime handlers with their own
    /// state directory, the configured one is used as default.
    fn render_config(
        config: &Config,
        dir: &Path,
        socket: &CriSocket,
        conmon: &str,
        paths: &RuntimePaths,
    ) -> String {
        // CRI-O validates runtime_path with stat; empty lets it resolve the
        // handler name from $PATH inside the node container.
        let runtimes = paths
            .runtimes
            .iter()
            .map(|(runtime, path)| {
                let path = if config.multi_node() && !config.is_rootless() {
                    ""
                } else {
                    path.as_str()
                };
                format!(
                    "[crio.runtime.runtimes.{name}]\n\
                     runtime_path = \"{path}\"\n\
                     runtime_root = \"{root}\"\n\
                     runtime_type = \"oci\"\n",
                    name = runtime,
                    root = dir.join(runtime.name()).display(),
                )
            })
            .collect::<Vec<_>>()
            .join("\n");
        let containers_dir = dir.join("containers");
        format!(
            include_str!("assets/crio.conf"),
            attach_socket_dir = dir.join("attach").display(),
            conmon = conmon,
            containers_root = containers_dir.join("storage").display(),
            containers_runroot = containers_dir.join("run").display(),
            listen = socket,
            log_dir = dir.join("log").display(),
            namespaces_dir = dir.join("ns").display(),
            network_dir = dir.join("cni").display(),
            plugin_dir = paths.plugin_dir,
            exits_dir = dir.join("exits").display(),
            default_runtime = config.oci_runtime(),
            runtimes = runtimes,
            signature_policy = Container::policy_json(config).display(),
            storage_driver = "overlay",
            storage_option = "",
            version_file = dir.join("version").display(),
            disable_hostport_mapping = config.is_rootless(),
            enable_nri = !config.is_rootless(),
        )
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
    use crate::config::OciRuntime;
    use clap::Parser;

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

    fn test_paths() -> RuntimePaths {
        RuntimePaths {
            runtimes: vec![
                (OciRuntime::Crun, "/nix/bin/crun".into()),
                (OciRuntime::Runc, "/nix/bin/runc".into()),
            ],
            plugin_dir: "/nix/cni".into(),
        }
    }

    fn render_with(config: &Config, paths: &RuntimePaths) -> Result<toml::Value> {
        let dir = PathBuf::from("/kubernix/crio/node");
        let socket = CriSocket::new(dir.join("crio.sock"))?;
        let content = Crio::render_config(config, &dir, &socket, "/bin/conmon", paths);
        Ok(toml::from_str(&content)?)
    }

    fn render(config: &Config) -> Result<toml::Value> {
        render_with(config, &test_paths())
    }

    #[test]
    fn render_config_skips_missing_runtime() -> Result<()> {
        let mut paths = test_paths();
        paths.runtimes.retain(|(r, _)| *r == OciRuntime::Crun);
        let v = render_with(&Config::parse_from(["kubernix"]), &paths)?;
        let runtimes = &v["crio"]["runtime"]["runtimes"];
        assert!(runtimes.get("crun").is_some());
        assert!(runtimes.get("runc").is_none());
        Ok(())
    }

    #[test]
    fn render_config_registers_both_runtimes() -> Result<()> {
        let v = render(&Config::parse_from(["kubernix"]))?;
        let runtime = &v["crio"]["runtime"];
        assert_eq!(runtime["default_runtime"].as_str(), Some("crun"));
        let crun = &runtime["runtimes"]["crun"];
        assert_eq!(crun["runtime_path"].as_str(), Some("/nix/bin/crun"));
        assert_eq!(
            crun["runtime_root"].as_str(),
            Some("/kubernix/crio/node/crun")
        );
        let runc = &runtime["runtimes"]["runc"];
        assert_eq!(runc["runtime_path"].as_str(), Some("/nix/bin/runc"));
        assert_eq!(
            runc["runtime_root"].as_str(),
            Some("/kubernix/crio/node/runc")
        );
        Ok(())
    }

    #[test]
    fn render_config_default_runc() -> Result<()> {
        let v = render(&Config::parse_from(["kubernix", "--oci-runtime=runc"]))?;
        assert_eq!(
            v["crio"]["runtime"]["default_runtime"].as_str(),
            Some("runc")
        );
        assert!(v["crio"]["runtime"]["runtimes"].get("crun").is_some());
        Ok(())
    }

    #[test]
    fn render_config_multi_node_resolves_from_path() -> Result<()> {
        let v = render(&Config::parse_from(["kubernix", "--nodes=2"]))?;
        let runtimes = &v["crio"]["runtime"]["runtimes"];
        assert_eq!(runtimes["crun"]["runtime_path"].as_str(), Some(""));
        assert_eq!(runtimes["runc"]["runtime_path"].as_str(), Some(""));
        Ok(())
    }
}
