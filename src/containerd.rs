//! containerd container runtime component.
//!
//! Manages containerd instances that provide the container runtime
//! interface for kubelet. Each node gets its own instance with a separate
//! CNI network configuration. In multi-node root mode, each instance runs
//! inside its own node container.

use crate::{
    Config,
    component::{ClusterContext, Component, Phase},
    config::OciRuntime,
    container::{Container, NodeContainer},
    cri::{self, CriSocket, RuntimePaths},
    network::Network,
    node::Node,
    process::{Process, ProcessState, Stoppable},
    write_if_changed,
};
use anyhow::{Context, Result};
use std::{
    fs::{self, create_dir_all},
    path::{Path, PathBuf},
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
    container: Option<NodeContainer>,
    node_name: String,
}

const CONTAINERD: &str = "containerd";

impl Containerd {
    /// Start a containerd instance for the given node index.
    pub fn start(config: &Config, node: u8, network: &Network) -> ProcessState {
        let node_name = Node::name(config, network, node);

        let paths = RuntimePaths::resolve(config)?;

        let dir = Self::path(config, network, node);
        let config_file = dir.join("config.toml");
        let cni_conf_dir = dir.join("cni");
        let socket = Self::socket(config, network, node)?;

        // Always render the configuration, since it references the current
        // Nix store paths.
        create_dir_all(&cni_conf_dir).context("Unable to create containerd CNI directory")?;

        // In rootless mode, kubelet sends oomScoreAdj via CRI, containerd
        // passes it to the OCI spec, and both crun and runc fail writing
        // /proc/self/oom_score_adj in a user namespace. Wrap each runtime
        // to patch config.json before exec (strip oomScoreAdj, fix
        // namespace config).
        if config.is_rootless() {
            for (runtime, path) in &paths.runtimes {
                let wrapper = Self::rootless_wrapper(&dir, *runtime);
                let script =
                    include_str!("assets/oci-rootless.sh").replace("__RUNTIME_PATH__", path);
                write_if_changed(&wrapper, &script)
                    .with_context(|| format!("Unable to write {} rootless wrapper", runtime))?;
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    fs::set_permissions(&wrapper, fs::Permissions::from_mode(0o755)).with_context(
                        || format!("Unable to set {} rootless wrapper permissions", runtime),
                    )?;
                }
            }
        }

        write_if_changed(
            &config_file,
            &Self::render_config(config, &dir, &socket, &cni_conf_dir, &paths),
        )
        .context("Unable to write containerd config")?;

        cri::write_cni_config(&cni_conf_dir, &node_name, node, network)?;
        if config.is_rootless() {
            cri::reset_ipam_data(&cni_conf_dir)?;
        }

        // containerd-shim-runc-v2 hardcodes its socket path to /run/containerd/s/.
        // System::setup recreates /run/containerd with the correct ownership
        // in rootless mode, before the nodes start in parallel.
        if config.is_rootless() {
            create_dir_all("/run/containerd/s").context("Unable to create /run/containerd/s")?;
        }

        let config_arg = format!("--config={}", config_file.display());
        let args: &[&str] = &[&config_arg];

        let mut process = if config.multi_node() && !config.is_rootless() {
            // Run inside a container
            let identifier = format!("Containerd {}", node_name);
            Container::start(config, &dir, &identifier, CONTAINERD, &node_name, args)?
        } else {
            Process::start(&dir, "Containerd", CONTAINERD, args)?
        };
        let container = Container::node(config, &node_name);
        process.wait_ready("containerd successfully booted")?;

        Ok(Box::new(Self {
            process,
            socket,
            container,
            node_name,
        }))
    }

    /// Path of the rootless wrapper script for the given OCI runtime.
    fn rootless_wrapper(dir: &Path, runtime: OciRuntime) -> PathBuf {
        dir.join(format!("{}-rootless", runtime))
    }

    /// Render the containerd configuration for the node working directory.
    ///
    /// Both OCI runtimes are registered as runtime handlers with their own
    /// state directory, the configured one is used as default.
    fn render_config(
        config: &Config,
        dir: &Path,
        socket: &CriSocket,
        cni_conf_dir: &Path,
        paths: &RuntimePaths,
    ) -> String {
        const RUNTIMES: &str = r#"plugins."io.containerd.cri.v1.runtime".containerd.runtimes"#;
        let runtimes = paths
            .runtimes
            .iter()
            .map(|(runtime, path)| {
                let path = if config.is_rootless() {
                    Self::rootless_wrapper(dir, *runtime).display().to_string()
                } else {
                    path.clone()
                };
                format!(
                    "[{RUNTIMES}.{name}]\n\
                     runtime_type = \"io.containerd.runc.v2\"\n\
                     \n\
                     [{RUNTIMES}.{name}.options]\n\
                     BinaryName = \"{path}\"\n\
                     Root = \"{root}\"\n\
                     SystemdCgroup = false\n",
                    name = runtime,
                    root = dir.join(runtime.name()).display(),
                )
            })
            .collect::<Vec<_>>()
            .join("\n");
        // containerd 2.3.3 native snapshotter fails with "no unpack
        // platforms defined"; overlayfs works in all modes.
        let snapshotter = "overlayfs";
        format!(
            include_str!("assets/containerd.toml"),
            root = dir.join("root").display(),
            state = dir.join("state").display(),
            socket = socket,
            plugin_dir = paths.plugin_dir,
            cni_conf_dir = cni_conf_dir.display(),
            default_runtime = config.oci_runtime(),
            runtimes = runtimes,
            snapshotter = snapshotter,
            disable_apparmor = config.is_rootless(),
            disable_nri = config.is_rootless(),
        )
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
        // Always stop the process, even if the workloads cannot be removed,
        // for example because containerd crashed.
        let removed = cri::remove_all_containers("containerd", &self.socket, &self.node_name)
            .with_context(|| {
                format!(
                    "Unable to remove containerd containers on {}",
                    self.node_name,
                )
            });
        let stopped = self.process.stop();
        if let Some(container) = &mut self.container {
            container.remove();
        }
        removed.and(stopped)
    }

    fn died(&mut self) -> bool {
        self.process.died()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

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

    #[test]
    fn render_config_skips_missing_runtime() -> Result<()> {
        let dir = PathBuf::from("/kubernix/containerd/node");
        let socket = CriSocket::new(dir.join("containerd.sock"))?;
        let paths = RuntimePaths {
            runtimes: vec![(OciRuntime::Runc, "/nix/bin/runc".into())],
            plugin_dir: "/nix/cni".into(),
        };
        let config = Config::parse_from(["kubernix", "--oci-runtime=runc"]);
        let content = Containerd::render_config(&config, &dir, &socket, &dir.join("cni"), &paths);
        let v: toml::Value = toml::from_str(&content)?;
        let containerd = cri_runtime(&v);
        assert_eq!(containerd["default_runtime_name"].as_str(), Some("runc"));
        assert!(containerd["runtimes"].get("runc").is_some());
        assert!(containerd["runtimes"].get("crun").is_none());
        Ok(())
    }

    fn render(config: &Config) -> Result<toml::Value> {
        let dir = PathBuf::from("/kubernix/containerd/node");
        let socket = CriSocket::new(dir.join("containerd.sock"))?;
        let paths = RuntimePaths {
            runtimes: vec![
                (OciRuntime::Crun, "/nix/bin/crun".into()),
                (OciRuntime::Runc, "/nix/bin/runc".into()),
            ],
            plugin_dir: "/nix/cni".into(),
        };
        let content = Containerd::render_config(config, &dir, &socket, &dir.join("cni"), &paths);
        Ok(toml::from_str(&content)?)
    }

    fn cri_runtime(v: &toml::Value) -> &toml::Value {
        &v["plugins"]["io.containerd.cri.v1.runtime"]["containerd"]
    }

    #[test]
    fn render_config_registers_both_runtimes() -> Result<()> {
        let v = render(&Config::parse_from(["kubernix"]))?;
        let containerd = cri_runtime(&v);
        assert_eq!(containerd["default_runtime_name"].as_str(), Some("crun"));
        for (name, path) in [("crun", "/nix/bin/crun"), ("runc", "/nix/bin/runc")] {
            let runtime = &containerd["runtimes"][name];
            assert_eq!(
                runtime["runtime_type"].as_str(),
                Some("io.containerd.runc.v2")
            );
            assert_eq!(runtime["options"]["BinaryName"].as_str(), Some(path));
            assert_eq!(
                runtime["options"]["Root"].as_str(),
                Some(format!("/kubernix/containerd/node/{}", name).as_str())
            );
        }
        Ok(())
    }

    #[test]
    fn render_config_current_version() -> Result<()> {
        let v = render(&Config::parse_from(["kubernix"]))?;
        assert_eq!(v["version"].as_integer(), Some(4));
        let plugins = &v["plugins"];
        assert_eq!(
            plugins["io.containerd.server.v1.grpc"]["address"].as_str(),
            Some("/kubernix/containerd/node/containerd.sock")
        );
        // Defaults to /run/containerd otherwise
        assert_eq!(
            plugins["io.containerd.server.v1.ttrpc"]["address"].as_str(),
            Some("/kubernix/containerd/node/containerd.sock.ttrpc")
        );
        let images = &plugins["io.containerd.cri.v1.images"];
        assert!(images.get("sandbox_image").is_none());
        assert_eq!(
            images["pinned_images"]["sandbox"].as_str(),
            Some("registry.k8s.io/pause:3.10.2")
        );
        Ok(())
    }

    #[test]
    fn render_config_default_runc() -> Result<()> {
        let v = render(&Config::parse_from(["kubernix", "--oci-runtime=runc"]))?;
        let containerd = cri_runtime(&v);
        assert_eq!(containerd["default_runtime_name"].as_str(), Some("runc"));
        assert!(containerd["runtimes"].get("crun").is_some());
        Ok(())
    }

    #[test]
    fn render_config_rootless_uses_wrappers() -> Result<()> {
        let mut c = Config::parse_from(["kubernix"]);
        c.set_rootless(true);
        let v = render(&c)?;
        let runtimes = &cri_runtime(&v)["runtimes"];
        for name in ["crun", "runc"] {
            assert_eq!(
                runtimes[name]["options"]["BinaryName"].as_str(),
                Some(format!("/kubernix/containerd/node/{}-rootless", name).as_str())
            );
        }
        Ok(())
    }
}
