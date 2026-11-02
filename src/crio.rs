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
use std::{
    fs::create_dir_all,
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
}

const CRIO: &str = "crio";
const CONMONRS: &str = "conmonrs";

/// Registries for short image names, which CRI-O resolves via the
/// containers-registries.conf(5) file. Like containerd, short names resolve
/// to docker.io.
const REGISTRIES_CONF: &str = "unqualified-search-registries = [\"docker.io\"]\n";

/// The containers-storage.conf(5) file, which replaces the one of the host.
/// The CRI-O configuration sets the storage paths.
const STORAGE_CONF: &str = "[storage]\ndriver = \"overlay\"\n";

/// Environment variable for the path of the containers-storage.conf(5) file.
const STORAGE_CONF_ENV: &str = "CONTAINERS_STORAGE_CONF";

/// Base directory of the temporary CRI-O files, which have to vanish on a
/// reboot. In rootless mode, `/run` belongs to the mount namespace of
/// rootlesskit, in multi-node root mode to the node container.
pub const RUN_BASE: &str = "/run/kubernix";

impl Crio {
    /// Start a CRI-O instance for the given node index.
    pub fn start(config: &Config, node: u8, network: &Network) -> ProcessState {
        let node_name = Node::name(config, network, node);

        let paths = RuntimePaths::resolve(config)?;
        let conmonrs = System::find_executable(CONMONRS)?.display().to_string();

        let dir = Self::path(config, network, node);
        let config_dir = dir.join("crio.conf.d");
        let config_file = config_dir.join("crio.conf");
        let registries_file = dir.join("registries.conf");
        let registries_dir = dir.join("registries.conf.d");
        let storage_file = dir.join("storage.conf");
        let network_dir = dir.join("cni");
        let socket = Self::socket(config, network, node)?;
        let run_dir = Self::run_dir(config, node);

        // Always render the configuration, since it references the current
        // Nix store paths.
        for d in [
            &network_dir,
            &config_dir,
            &registries_dir,
            &dir.join("ns"),
            &dir.join("hooks.d"),
            &dir.join("cdi"),
            &dir.join("tmp"),
        ] {
            create_dir_all(d)
                .with_context(|| format!("Unable to create CRI-O directory {}", d.display()))?;
        }
        if config.is_rootless() {
            cri::write_rootless_wrappers(&dir, &paths)?;
        }
        write_if_changed(
            &config_file,
            &Self::render_config(config, &dir, &run_dir, &socket, &conmonrs, &paths),
        )
        .context("Unable to write CRI-O config")?;
        write_if_changed(&registries_file, REGISTRIES_CONF)
            .context("Unable to write CRI-O registries config")?;
        write_if_changed(&storage_file, STORAGE_CONF)
            .context("Unable to write CRI-O storage config")?;
        // CRI-O maps the host ports of the pods itself
        cri::write_cni_config(&network_dir, &node_name, node, network, false)?;
        cri::reset_ipam_data(&network_dir)?;
        let args = Self::args(&config_dir, &registries_file, &registries_dir);
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        let storage_file = storage_file.display().to_string();
        let env = [(STORAGE_CONF_ENV, storage_file.as_str())];

        let mut process = if config.multi_node() && !config.is_rootless() {
            // Run inside a container
            let identifier = format!("CRI-O {}", node_name);
            Container::start(config, &dir, &identifier, CRIO, &node_name, &args, &env)?
        } else {
            Process::start_with_env(&dir, "CRI-O", CRIO, &args, &env)?
        };
        let container = Container::node(config, &node_name);
        process.wait_ready("No systemd watchdog enabled")?;

        Ok(Box::new(Self {
            process,
            socket,
            container,
            node_name,
        }))
    }

    /// The command line arguments of CRI-O. The main configuration file
    /// defaults to the one of the host, so an empty one replaces it and the
    /// configuration comes from the drop-in directory of the node. Short
    /// image names get resolved via the registries config of the node,
    /// without the drop-in files of the host.
    fn args(config_dir: &Path, registries_file: &Path, registries_dir: &Path) -> Vec<String> {
        vec![
            "--config=/dev/null".into(),
            format!("--config-dir={}", config_dir.display()),
            format!("--registries-conf={}", registries_file.display()),
            format!("--registries-conf-dir={}", registries_dir.display()),
        ]
    }

    /// Render the CRI-O configuration for the node working directory `dir`
    /// and the temporary directory `run_dir`.
    ///
    /// Both OCI runtimes are registered as runtime handlers with their own
    /// state directory, the configured one is used as default.
    ///
    /// The handlers use the pod runtime type, where one conmon-rs instance
    /// per pod monitors the containers and also provides their attach
    /// sockets below the directory of the infra container. CRI-O 1.37
    /// creates that instance for the infra container of the pod and
    /// replaces a dropped infra container by a placeholder without runtime
    /// handler, which would run all containers of the pod with the default
    /// handler instead of the one of their RuntimeClass, so the infra
    /// container is kept (`drop_infra_ctr = false`). Every pod has an infra
    /// container then, and every node pulls the pause image. Its OOM score
    /// of -998 cannot be set in a user namespace, so rootless mode uses the
    /// runtime wrappers which raise it (see `cri::write_rootless_wrappers`).
    /// Both workarounds become unnecessary once CRI-O passes the runtime
    /// handler of the pod to the conmon-rs instance of a dropped infra
    /// container, and skips or clamps the OOM score of the infra container
    /// in a user namespace.
    ///
    /// The capabilities and sysctls of the containers match the defaults of
    /// containerd. NRI is disabled, and the other paths CRI-O uses belong to
    /// the node instead of the host, also the CDI spec directory, which is
    /// empty. CRI-O creates the attach socket directory and the PID files of
    /// exec sessions below `run_dir`, otherwise it would use `/var/run/crio`.
    /// Only the registry certificates (`certs.d`) and credentials of the host
    /// still apply, like for other image pulls on the host.
    fn render_config(
        config: &Config,
        dir: &Path,
        run_dir: &Path,
        socket: &CriSocket,
        conmonrs: &str,
        paths: &RuntimePaths,
    ) -> String {
        let runtimes = paths
            .handlers(config, dir)
            .iter()
            .map(|(runtime, path)| {
                format!(
                    "[crio.runtime.runtimes.{name}]\n\
                     runtime_path = \"{path}\"\n\
                     runtime_root = \"{root}\"\n\
                     runtime_type = \"pod\"\n\
                     monitor_path = \"{conmonrs}\"\n\
                     monitor_cgroup = \"pod\"\n",
                    name = runtime,
                    root = dir.join(runtime.name()).display(),
                )
            })
            .collect::<Vec<_>>()
            .join("\n");
        let containers_dir = dir.join("containers");
        format!(
            include_str!("assets/crio.conf"),
            attach_socket_dir = run_dir.display(),
            big_files_temporary_dir = dir.join("tmp").display(),
            cdi_spec_dir = dir.join("cdi").display(),
            clean_shutdown_file = dir.join("clean.shutdown").display(),
            containers_root = containers_dir.join("storage").display(),
            containers_runroot = containers_dir.join("run").display(),
            decryption_keys_path = dir.join("keys").display(),
            default_mounts_file = dir.join("mounts.conf").display(),
            default_sysctls = Self::default_sysctls(config),
            hooks_dir = dir.join("hooks.d").display(),
            irqbalance_config_file = dir.join("irqbalance").display(),
            listen = socket,
            log_dir = dir.join("log").display(),
            namespaces_dir = dir.join("ns").display(),
            network_dir = dir.join("cni").display(),
            plugin_dir = paths.plugin_dir,
            exits_dir = dir.join("exits").display(),
            default_runtime = config.oci_runtime(),
            runtimes = runtimes,
            signature_policy = Container::policy_json(config).display(),
            signature_policy_dir = dir.join("policies").display(),
            storage_driver = "overlay",
            storage_option = "",
            version_file = run_dir.join("version").display(),
        )
    }

    /// The default sysctls of the containers as TOML array items, like
    /// containerd sets them for pods outside of the host network.
    /// containerd skips the ICMP echo socket range in a user namespace, in
    /// which the groups up to 2147483647 are not mapped.
    fn default_sysctls(config: &Config) -> String {
        let mut sysctls = vec!["net.ipv4.ip_unprivileged_port_start=0"];
        if !config.is_rootless() {
            sysctls.push("net.ipv4.ping_group_range=0 2147483647");
        }
        let items: String = sysctls
            .iter()
            .map(|s| format!("\n    \"{}\",", s))
            .collect();
        items + "\n"
    }

    /// Retrieve the CRI socket
    pub fn socket(config: &Config, network: &Network, node: u8) -> Result<CriSocket> {
        CriSocket::new(Self::path(config, network, node).join("crio.sock"))
    }

    /// Retrieve the temporary directory of the node, which is unique per run
    /// root and node.
    fn run_dir(config: &Config, node: u8) -> PathBuf {
        PathBuf::from(RUN_BASE)
            .join(System::root_id(config.root()))
            .join(node.to_string())
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
        cri::kill_leftover_monitors(&self.socket, &self.node_name);
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
    use crate::config::OciRuntime;

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

    /// The absolute run root of the test configurations. A relative one
    /// would hide paths of the configuration which do not belong to the
    /// node.
    const ROOT: &str = "/kubernix";

    /// Parse the arguments with the run root `ROOT`.
    fn parse(args: &[&str]) -> Result<Config> {
        let root = format!("--root={}", ROOT);
        let args: Vec<&str> = [root.as_str()]
            .into_iter()
            .chain(args.iter().copied())
            .collect();
        crate::config::tests::parse(&args)
    }

    fn render_with(config: &Config, paths: &RuntimePaths) -> Result<toml::Value> {
        let dir = config.root().join(CRIO).join("node");
        let socket = CriSocket::new(dir.join("crio.sock"))?;
        let run_dir = Crio::run_dir(config, 0);
        let content =
            Crio::render_config(config, &dir, &run_dir, &socket, "/nix/bin/conmonrs", paths);
        Ok(toml::from_str(&content)?)
    }

    fn render(config: &Config) -> Result<toml::Value> {
        render_with(config, &test_paths())
    }

    fn config(args: &[&str], rootless: bool) -> Config {
        let mut c = parse(&args[1..]).expect("invalid arguments");
        c.set_rootless(rootless);
        c
    }

    #[test]
    fn render_config_skips_missing_runtime() -> Result<()> {
        let mut paths = test_paths();
        paths.runtimes.retain(|(r, _)| *r == OciRuntime::Crun);
        let v = render_with(&parse(&[])?, &paths)?;
        let runtimes = &v["crio"]["runtime"]["runtimes"];
        assert!(runtimes.get("crun").is_some());
        assert!(runtimes.get("runc").is_none());
        Ok(())
    }

    #[test]
    fn render_config_registers_both_runtimes() -> Result<()> {
        let v = render(&config(&["kubernix"], false))?;
        let runtime = &v["crio"]["runtime"];
        assert_eq!(runtime["default_runtime"].as_str(), Some("crun"));
        let crun = &runtime["runtimes"]["crun"];
        assert_eq!(crun["runtime_path"].as_str(), Some("/nix/bin/crun"));
        assert_eq!(crun["monitor_path"].as_str(), Some("/nix/bin/conmonrs"));
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
        let v = render(&parse(&["--oci-runtime=runc"])?)?;
        assert_eq!(
            v["crio"]["runtime"]["default_runtime"].as_str(),
            Some("runc")
        );
        assert!(v["crio"]["runtime"]["runtimes"].get("crun").is_some());
        Ok(())
    }

    #[test]
    fn render_config_uses_conmonrs() -> Result<()> {
        for args in [&["kubernix"][..], &["kubernix", "--nodes=2"]] {
            for rootless in [false, true] {
                let v = render(&config(args, rootless))?;
                for name in ["crun", "runc"] {
                    let runtime = &v["crio"]["runtime"]["runtimes"][name];
                    assert_eq!(runtime["runtime_type"].as_str(), Some("pod"));
                    assert_eq!(runtime["monitor_path"].as_str(), Some("/nix/bin/conmonrs"));
                    assert_eq!(runtime["monitor_cgroup"].as_str(), Some("pod"));
                    assert!(runtime.get("monitor_env").is_none());
                }
                // Required for RuntimeClasses with the pod runtime type
                assert_eq!(
                    v["crio"]["runtime"]["drop_infra_ctr"].as_bool(),
                    Some(false)
                );
            }
        }
        Ok(())
    }

    #[test]
    fn render_config_rootless_uses_wrappers() -> Result<()> {
        for args in [&["kubernix"][..], &["kubernix", "--nodes=2"]] {
            let v = render(&config(args, true))?;
            for name in ["crun", "runc"] {
                assert_eq!(
                    v["crio"]["runtime"]["runtimes"][name]["runtime_path"].as_str(),
                    Some(format!("/kubernix/crio/node/{}-rootless", name).as_str())
                );
            }
        }
        Ok(())
    }

    #[test]
    fn render_config_skips_removed_options() -> Result<()> {
        let v = render(&parse(&[])?)?;
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
    fn render_config_uses_node_paths() -> Result<()> {
        for rootless in [false, true] {
            let c = config(&["kubernix"], rootless);
            let run_dir = Crio::run_dir(&c, 0).display().to_string();
            assert!(run_dir.starts_with("/run/kubernix/"));
            let version_file = format!("{}/version", run_dir);
            let v = render(&c)?;
            let crio = &v["crio"];
            let runtime = &crio["runtime"];
            for (value, expected) in [
                (
                    &crio["clean_shutdown_file"],
                    "/kubernix/crio/node/clean.shutdown",
                ),
                (&crio["version_file"], version_file.as_str()),
                (&runtime["decryption_keys_path"], "/kubernix/crio/node/keys"),
                (
                    &runtime["default_mounts_file"],
                    "/kubernix/crio/node/mounts.conf",
                ),
                (
                    &runtime["irqbalance_config_file"],
                    "/kubernix/crio/node/irqbalance",
                ),
                (&runtime["irqbalance_config_restore_file"], "disable"),
                (&runtime["container_attach_socket_dir"], run_dir.as_str()),
                (&runtime["container_exits_dir"], "/kubernix/crio/node/exits"),
                (&runtime["namespaces_dir"], "/kubernix/crio/node/ns"),
                (
                    &crio["image"]["signature_policy_dir"],
                    "/kubernix/crio/node/policies",
                ),
                (
                    &crio["image"]["big_files_temporary_dir"],
                    "/kubernix/crio/node/tmp",
                ),
            ] {
                assert_eq!(value.as_str(), Some(expected));
            }
            assert_eq!(
                runtime["hooks_dir"],
                toml::Value::Array(vec!["/kubernix/crio/node/hooks.d".into()])
            );
            assert_eq!(
                runtime["cdi_spec_dirs"],
                toml::Value::Array(vec!["/kubernix/crio/node/cdi".into()])
            );
            assert_eq!(crio["nri"]["enable_nri"].as_bool(), Some(false));
            assert_eq!(runtime["log_level"].as_str(), Some("info"));
        }
        Ok(())
    }

    /// Collect the absolute paths of the configuration with their keys.
    fn absolute_paths<'a>(key: &'a str, v: &'a toml::Value, paths: &mut Vec<(&'a str, &'a str)>) {
        match v {
            toml::Value::String(s) if s.starts_with('/') => paths.push((key, s)),
            toml::Value::Array(a) => a.iter().for_each(|v| absolute_paths(key, v, paths)),
            toml::Value::Table(t) => t.iter().for_each(|(k, v)| absolute_paths(k, v, paths)),
            _ => {}
        }
    }

    /// The options whose default in CRI-O 1.37 (`crio config --default`)
    /// belongs to the host, as table and key. An empty
    /// `default_mounts_file`, `signature_policy` or `big_files_temporary_dir`
    /// falls back to the host as well. The NRI options are left out, since
    /// NRI is disabled, and the global registry credentials of the host
    /// (`global_auth_file`) apply on purpose.
    const HOST_DEFAULTS: &[(&str, &str)] = &[
        ("crio", "root"),
        ("crio", "runroot"),
        ("crio", "log_dir"),
        ("crio", "version_file"),
        ("crio", "clean_shutdown_file"),
        ("crio.api", "listen"),
        ("crio.runtime", "decryption_keys_path"),
        ("crio.runtime", "irqbalance_config_file"),
        ("crio.runtime", "irqbalance_config_restore_file"),
        ("crio.runtime", "cdi_spec_dirs"),
        ("crio.runtime", "hooks_dir"),
        ("crio.runtime", "default_mounts_file"),
        ("crio.runtime", "container_exits_dir"),
        ("crio.runtime", "container_attach_socket_dir"),
        ("crio.runtime", "namespaces_dir"),
        ("crio.image", "signature_policy"),
        ("crio.image", "signature_policy_dir"),
        ("crio.image", "big_files_temporary_dir"),
        ("crio.network", "network_dir"),
        ("crio.network", "plugin_dirs"),
    ];

    /// Returns true if the path belongs to the node: it is below the run
    /// root, the temporary directory of the node or the Nix store (`/nix/`
    /// in the tests).
    fn node_path(config: &Config, path: &str) -> bool {
        let path = Path::new(path);
        path.starts_with(config.root())
            || path.starts_with(Crio::run_dir(config, 0))
            || path.starts_with("/nix/")
    }

    #[test]
    fn render_config_overrides_host_defaults() -> Result<()> {
        for args in [&["kubernix"][..], &["kubernix", "--nodes=2"]] {
            for rootless in [false, true] {
                let c = config(args, rootless);
                let v = render(&c)?;
                for (table, key) in HOST_DEFAULTS {
                    let value = table
                        .split('.')
                        .try_fold(&v, |v, k| v.get(k))
                        .and_then(|t| t.get(key))
                        .with_context(|| format!("{}.{} is not set", table, key))?;
                    let values: Vec<_> = match value {
                        toml::Value::Array(a) => a.iter().collect(),
                        value => vec![value],
                    };
                    assert!(!values.is_empty(), "{}.{} is empty", table, key);
                    for value in values {
                        let value = value
                            .as_str()
                            .with_context(|| format!("{}.{} is no string", table, key))?;
                        // Skips the restore of the irqbalance config
                        if *key == "irqbalance_config_restore_file" {
                            assert_eq!(value, "disable");
                            continue;
                        }
                        assert!(
                            node_path(&c, value),
                            "{}.{} = {} belongs to the host",
                            table,
                            key,
                            value,
                        );
                    }
                }
                assert_eq!(v["crio"]["nri"]["enable_nri"].as_bool(), Some(false));
            }
        }
        Ok(())
    }

    #[test]
    fn render_config_uses_no_host_paths() -> Result<()> {
        for args in [&["kubernix"][..], &["kubernix", "--nodes=2"]] {
            for rootless in [false, true] {
                let c = config(args, rootless);
                assert!(c.root().is_absolute());
                let v = render(&c)?;
                let mut paths = Vec::new();
                absolute_paths("", &v, &mut paths);
                assert!(paths.len() > HOST_DEFAULTS.len());
                for (key, path) in paths {
                    // The command inside of the pause image
                    if key == "pause_command" {
                        continue;
                    }
                    assert!(
                        node_path(&c, path),
                        "{} = {} belongs to the host",
                        key,
                        path,
                    );
                }
            }
        }
        Ok(())
    }

    #[test]
    fn render_config_matches_containerd_defaults() -> Result<()> {
        let v = render(&config(&["kubernix"], false))?;
        let runtime = &v["crio"]["runtime"];
        let caps: Vec<_> = runtime["default_capabilities"]
            .as_array()
            .context("no capabilities")?
            .iter()
            .filter_map(|c| c.as_str())
            .collect();
        assert_eq!(caps.len(), 14);
        for cap in ["MKNOD", "NET_RAW", "SETFCAP", "SYS_CHROOT", "AUDIT_WRITE"] {
            assert!(caps.contains(&cap), "{} missing", cap);
        }
        assert_eq!(
            runtime["default_sysctls"],
            toml::Value::Array(vec![
                "net.ipv4.ip_unprivileged_port_start=0".into(),
                "net.ipv4.ping_group_range=0 2147483647".into(),
            ])
        );
        assert_eq!(
            v["crio"]["image"]["short_name_mode"].as_str(),
            Some("disabled")
        );
        // The groups are not mapped in the user namespace
        let v = render(&config(&["kubernix"], true))?;
        assert_eq!(
            v["crio"]["runtime"]["default_sysctls"],
            toml::Value::Array(vec!["net.ipv4.ip_unprivileged_port_start=0".into()])
        );
        Ok(())
    }

    #[test]
    fn args_skip_host_config() {
        let args = Crio::args(
            Path::new("/n/crio.conf.d"),
            Path::new("/n/registries.conf"),
            Path::new("/n/registries.conf.d"),
        );
        assert_eq!(
            args,
            [
                "--config=/dev/null",
                "--config-dir=/n/crio.conf.d",
                "--registries-conf=/n/registries.conf",
                "--registries-conf-dir=/n/registries.conf.d",
            ]
        );
    }

    #[test]
    fn registries_conf_resolves_short_names() -> Result<()> {
        let v: toml::Value = toml::from_str(REGISTRIES_CONF)?;
        assert_eq!(
            v["unqualified-search-registries"],
            toml::Value::Array(vec!["docker.io".into()])
        );
        Ok(())
    }

    #[test]
    fn storage_conf_sets_driver() -> Result<()> {
        let v: toml::Value = toml::from_str(STORAGE_CONF)?;
        assert_eq!(v["storage"]["driver"].as_str(), Some("overlay"));
        Ok(())
    }

    #[test]
    fn run_dir_unique_per_root_and_node() -> Result<()> {
        let a = crate::config::tests::parse(&["--root", "/home/user/kubernix-run"])?;
        let b = crate::config::tests::parse(&["--root", "/home/user/other-run"])?;
        assert!(Crio::run_dir(&a, 0).starts_with(RUN_BASE));
        assert_ne!(Crio::run_dir(&a, 0), Crio::run_dir(&a, 1));
        assert_ne!(Crio::run_dir(&a, 0), Crio::run_dir(&b, 0));
        assert_eq!(Crio::run_dir(&a, 0), Crio::run_dir(&a, 0));
        Ok(())
    }
}
