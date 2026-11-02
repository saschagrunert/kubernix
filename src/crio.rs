//! CRI-O container runtime component.
//!
//! Manages CRI-O instances that provide the container runtime interface
//! for kubelet. Each node gets its own instance with a separate CNI
//! network configuration. In multi-node root mode, each instance runs
//! inside its own node container.

use crate::{
    Config,
    component::{ClusterContext, Component, Phase},
    container::{Container, NodeContainer},
    cri::{self, CriSocket, RuntimePaths},
    network::Network,
    node::Node,
    process::{Process, ProcessState, Stoppable},
    system::System,
    write_if_changed,
};
use anyhow::{Context, Result};
use log::debug;
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
    container: Option<NodeContainer>,
    node_name: String,
    attach_socket_dir: PathBuf,
}

const CRIO: &str = "crio";
const CONMON: &str = "conmon";

/// Registries for short image names, which CRI-O resolves via the
/// containers-registries.conf(5) file.
const REGISTRIES_CONF: &str = "unqualified-search-registries = [\"docker.io\", \"quay.io\"]\n";

/// Base directory of the container attach sockets. CRI-O creates them at
/// `<dir>/<container id>/attach`, which exceeds the unix socket path length
/// limit for most directories below the run root. In rootless mode, `/run`
/// belongs to the mount namespace of rootlesskit, in multi-node root mode to
/// the node container.
pub const ATTACH_SOCKET_BASE: &str = "/run/kubernix";

impl Crio {
    /// Start a CRI-O instance for the given node index.
    pub fn start(config: &Config, node: u8, network: &Network) -> ProcessState {
        let node_name = Node::name(config, network, node);

        let paths = RuntimePaths::resolve(config)?;
        let monitor = System::find_executable(CONMON)?.display().to_string();

        let dir = Self::path(config, network, node);
        let config_dir = dir.join("crio.conf.d");
        let config_file = config_dir.join("crio.conf");
        let registries_file = dir.join("registries.conf");
        let network_dir = dir.join("cni");
        let socket = Self::socket(config, network, node)?;
        let attach_socket_dir = Self::attach_socket_dir(config, node);

        // Always render the configuration, since it references the current
        // Nix store paths.
        create_dir_all(&network_dir).context("Unable to create CRI-O CNI directory")?;
        create_dir_all(&config_dir).context("Unable to create CRI-O config directory")?;
        create_dir_all(dir.join("ns")).context("Unable to create CRI-O namespace directory")?;
        write_if_changed(
            &config_file,
            &Self::render_config(config, &dir, &socket, &attach_socket_dir, &monitor, &paths),
        )
        .context("Unable to write CRI-O config")?;
        write_if_changed(&registries_file, REGISTRIES_CONF)
            .context("Unable to write CRI-O registries config")?;
        cri::write_cni_config(&network_dir, &node_name, node, network)?;

        if config.is_rootless() {
            cri::reset_ipam_data(&network_dir)?;
        }
        let config_dir_arg = format!("--config-dir={}", config_dir.display());
        // Short image names get resolved via the registries config, which
        // does not depend on the one of the host.
        let registries_arg = format!("--registries-conf={}", registries_file.display());
        let args: &[&str] = &[&config_dir_arg, &registries_arg];

        let mut process = if config.multi_node() && !config.is_rootless() {
            // Run inside a container
            let identifier = format!("CRI-O {}", node_name);
            Container::start(config, &dir, &identifier, CRIO, &node_name, args)?
        } else {
            Process::start(&dir, "CRI-O", CRIO, args)?
        };
        let container = Container::node(config, &node_name);
        process.wait_ready("No systemd watchdog enabled")?;

        Ok(Box::new(Self {
            process,
            socket,
            container,
            node_name,
            attach_socket_dir,
        }))
    }

    /// Render the CRI-O configuration for the node working directory.
    ///
    /// Both OCI runtimes are registered as runtime handlers with their own
    /// state directory, the configured one is used as default. conmon gets
    /// configured per handler, which replaces the deprecated global options.
    fn render_config(
        config: &Config,
        dir: &Path,
        socket: &CriSocket,
        attach_socket_dir: &Path,
        monitor: &str,
        paths: &RuntimePaths,
    ) -> String {
        let runtimes = paths
            .runtimes
            .iter()
            .map(|(runtime, path)| {
                format!(
                    "[crio.runtime.runtimes.{name}]\n\
                     runtime_path = \"{path}\"\n\
                     runtime_root = \"{root}\"\n\
                     runtime_type = \"oci\"\n\
                     monitor_path = \"{monitor}\"\n\
                     monitor_cgroup = \"pod\"\n\
                     monitor_env = [\n    \
                     \"PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin\",\n\
                     ]\n",
                    name = runtime,
                    root = dir.join(runtime.name()).display(),
                )
            })
            .collect::<Vec<_>>()
            .join("\n");
        let containers_dir = dir.join("containers");
        format!(
            include_str!("assets/crio.conf"),
            attach_socket_dir = attach_socket_dir.display(),
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
            enable_nri = !config.is_rootless(),
        )
    }

    /// Retrieve the CRI socket
    pub fn socket(config: &Config, network: &Network, node: u8) -> Result<CriSocket> {
        CriSocket::new(Self::path(config, network, node).join("crio.sock"))
    }

    /// Retrieve the attach socket directory for the node, which is unique per
    /// run root and node. CRI-O creates it on startup.
    fn attach_socket_dir(config: &Config, node: u8) -> PathBuf {
        PathBuf::from(ATTACH_SOCKET_BASE)
            .join(System::root_id(config.root()))
            .join(node.to_string())
    }

    /// Remove the attach socket directory of the node and its parents, as
    /// long as they are empty.
    fn remove_attach_socket_dir(&self) {
        if let Err(e) = fs::remove_dir_all(&self.attach_socket_dir)
            && self.attach_socket_dir.exists()
        {
            debug!(
                "Unable to remove '{}': {}",
                self.attach_socket_dir.display(),
                e
            );
        }
        for dir in self.attach_socket_dir.ancestors().skip(1).take(2) {
            if fs::remove_dir(dir).is_err() {
                break;
            }
        }
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
        // Always stop the process, even if the workloads cannot be removed,
        // for example because CRI-O crashed.
        let removed = cri::remove_all_containers("CRI-O", &self.socket, &self.node_name)
            .with_context(|| format!("Unable to remove CRI-O containers on {}", self.node_name));
        let stopped = self.process.stop();
        if let Some(container) = &mut self.container {
            container.remove();
        }
        // A CRI-O which is still running may use the sockets
        if stopped.is_ok() {
            self.remove_attach_socket_dir();
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
    use crate::{config::OciRuntime, cri::MAX_SOCKET_PATH_LEN};
    use clap::Parser;

    /// Container ID and file name of an attach socket below its directory.
    const ATTACH_SOCKET_SUFFIX_LEN: usize = "/".len() + 64 + "/attach".len();

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
        let attach = PathBuf::from("/run/kubernix/0123abcd/0");
        let content = Crio::render_config(config, &dir, &socket, &attach, "/nix/bin/conmon", paths);
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
        assert_eq!(crun["monitor_path"].as_str(), Some("/nix/bin/conmon"));
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
    fn render_config_configures_conmon_per_handler() -> Result<()> {
        for args in [&["kubernix"][..], &["kubernix", "--nodes=2"]] {
            for rootless in [false, true] {
                let mut c = Config::parse_from(args);
                c.set_rootless(rootless);
                let v = render(&c)?;
                for name in ["crun", "runc"] {
                    let runtime = &v["crio"]["runtime"]["runtimes"][name];
                    assert_eq!(runtime["runtime_type"].as_str(), Some("oci"));
                    assert_eq!(runtime["monitor_path"].as_str(), Some("/nix/bin/conmon"));
                    assert_eq!(runtime["monitor_cgroup"].as_str(), Some("pod"));
                    assert_eq!(runtime["monitor_env"].as_array().map(Vec::len), Some(1));
                }
            }
        }
        Ok(())
    }

    #[test]
    fn render_config_skips_removed_options() -> Result<()> {
        let v = render(&Config::parse_from(["kubernix"]))?;
        let runtime = &v["crio"]["runtime"];
        for key in [
            "conmon",
            "conmon_cgroup",
            "conmon_env",
            "default_mounts",
            "pids_limit",
            "log_size_max",
            "uid_mappings",
            "gid_mappings",
        ] {
            assert!(runtime.get(key).is_none(), "{} is set", key);
        }
        let image = &v["crio"]["image"];
        assert!(image.get("registries").is_none());
        assert_eq!(
            image["pause_image"].as_str(),
            Some("registry.k8s.io/pause:3.10.2")
        );
        Ok(())
    }

    #[test]
    fn registries_conf_resolves_short_names() -> Result<()> {
        let v: toml::Value = toml::from_str(REGISTRIES_CONF)?;
        assert_eq!(
            v["unqualified-search-registries"],
            toml::Value::Array(vec!["docker.io".into(), "quay.io".into()])
        );
        Ok(())
    }

    #[test]
    fn render_config_attach_socket_dir() -> Result<()> {
        let v = render(&Config::parse_from(["kubernix"]))?;
        assert_eq!(
            v["crio"]["runtime"]["container_attach_socket_dir"].as_str(),
            Some("/run/kubernix/0123abcd/0")
        );
        Ok(())
    }

    #[test]
    fn attach_socket_dir_fits_socket_path_limit() {
        let root = format!("/{}", "a".repeat(4000));
        let config = Config::parse_from(["kubernix", "--root", &root]);
        for node in [0, 9, 63, u8::MAX] {
            let dir = Crio::attach_socket_dir(&config, node);
            assert!(dir.starts_with(ATTACH_SOCKET_BASE));
            assert!(dir.as_os_str().len() + ATTACH_SOCKET_SUFFIX_LEN <= MAX_SOCKET_PATH_LEN);
        }
    }

    #[test]
    fn attach_socket_dir_unique_per_root_and_node() {
        let a = Config::parse_from(["kubernix", "--root", "/home/user/kubernix-run"]);
        let b = Config::parse_from(["kubernix", "--root", "/home/user/other-run"]);
        assert_ne!(
            Crio::attach_socket_dir(&a, 0),
            Crio::attach_socket_dir(&a, 1)
        );
        assert_ne!(
            Crio::attach_socket_dir(&a, 0),
            Crio::attach_socket_dir(&b, 0)
        );
        assert_eq!(
            Crio::attach_socket_dir(&a, 0),
            Crio::attach_socket_dir(&a, 0)
        );
    }
}
