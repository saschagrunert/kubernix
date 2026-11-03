//! Shared CRI (Container Runtime Interface) abstractions.
//!
//! Contains types and helpers used by both CRI-O and containerd
//! implementations.

use crate::{
    config::{Config, CriRuntime, OciRuntime},
    containerd::Containerd,
    crio::Crio,
    network::Network,
    process, shell,
    system::System,
    write_file, write_if_changed,
};
use anyhow::{Context, Result, bail};
use log::{debug, warn};
use nix::sys::signal::Signal;
use serde_json::{json, to_string_pretty};
use std::{
    ffi::OsStr,
    fmt::{self, Display, Formatter},
    fs,
    path::{Path, PathBuf},
    thread,
    time::Duration,
};

/// Environment variable name for the CRI socket endpoint.
pub const RUNTIME_ENV: &str = "CONTAINER_RUNTIME_ENDPOINT";

/// Resolved paths for the OCI runtimes and CNI plugins, shared between
/// CRI-O and containerd startup.
pub struct RuntimePaths {
    /// Available OCI runtimes and their executable paths, registered as
    /// runtime handlers.
    pub runtimes: Vec<(OciRuntime, String)>,
    pub plugin_dir: String,
}

impl RuntimePaths {
    /// Resolve the OCI runtime and CNI plugin paths. Node containers mount
    /// the Nix store of the host, so the paths are valid in all modes.
    pub fn resolve(config: &Config) -> Result<Self> {
        let runtimes = Self::available_runtimes(config.oci_runtime(), |r| {
            Ok(System::find_executable(r.name())?.display().to_string())
        })?;
        let loopback = System::find_executable("loopback")?;
        let plugin_dir = loopback
            .parent()
            .context("Unable to find CNI plugin dir")?
            .display()
            .to_string();
        Ok(Self {
            runtimes,
            plugin_dir,
        })
    }

    /// The OCI runtimes with the executable which the CRI runtime runs: the
    /// rootless wrapper in the node directory `dir` in rootless mode (see
    /// [`write_rootless_wrappers`]), otherwise the runtime itself.
    pub fn handlers(&self, config: &Config, dir: &Path) -> Vec<(OciRuntime, String)> {
        self.runtimes
            .iter()
            .map(|(runtime, path)| {
                let path = if config.is_rootless() {
                    rootless_wrapper(dir, *runtime).display().to_string()
                } else {
                    path.clone()
                };
                (*runtime, path)
            })
            .collect()
    }

    /// Look up all OCI runtimes. A missing default runtime is an error,
    /// while other missing runtimes are skipped with a warning, for
    /// example in an environment created by an older kubernix version.
    fn available_runtimes<F>(default: OciRuntime, find: F) -> Result<Vec<(OciRuntime, String)>>
    where
        F: Fn(OciRuntime) -> Result<String>,
    {
        let mut runtimes = vec![];
        for runtime in OciRuntime::ALL {
            match find(runtime) {
                Ok(path) => runtimes.push((runtime, path)),
                Err(e) if runtime == default => {
                    return Err(e.context(format!(
                        "Unable to find the default OCI runtime '{}'",
                        runtime
                    )));
                }
                Err(e) => warn!(
                    "Not registering the OCI runtime handler '{}': {:#}",
                    runtime, e
                ),
            }
        }
        Ok(runtimes)
    }
}

/// Path of the rootless wrapper script for the given OCI runtime.
pub fn rootless_wrapper(dir: &Path, runtime: OciRuntime) -> PathBuf {
    dir.join(format!("{}-rootless", runtime))
}

/// Write a wrapper script for every OCI runtime, used as runtime path in
/// rootless mode. The wrapper patches the `config.json` of the bundle
/// before running the runtime:
///
/// - Lowering the OOM score is not permitted in a user namespace, so both
///   crun and runc fail writing `/proc/self/oom_score_adj` for the values
///   which the kubelet or the CRI runtime (like for the infra container)
///   set. The wrapper raises lower values to its own OOM score, which keeps
///   the higher scores of BestEffort and Burstable pods.
/// - It drops namespace paths which point to the namespaces of the runtime
///   itself, which it cannot join.
/// - It adds a private UTS namespace, which setting the hostname requires,
///   unless one is left.
///
/// The wrapper fails if it cannot patch the `config.json`. It runs jq and
/// the runtime by their absolute paths, so it does not depend on the `PATH`
/// of the CRI runtime.
pub fn write_rootless_wrappers(dir: &Path, paths: &RuntimePaths) -> Result<()> {
    let jq = System::find_executable("jq")?;
    write_rootless_wrappers_with(dir, paths, &jq.display().to_string())
}

fn write_rootless_wrappers_with(dir: &Path, paths: &RuntimePaths, jq: &str) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    for (runtime, path) in &paths.runtimes {
        let wrapper = rootless_wrapper(dir, *runtime);
        let script = include_str!("assets/oci-rootless.sh")
            .replace("__JQ_PATH__", &shell::quote(jq))
            .replace("__RUNTIME_PATH__", &shell::quote(path));
        write_if_changed(&wrapper, &script)
            .with_context(|| format!("Unable to write {} rootless wrapper", runtime))?;
        fs::set_permissions(&wrapper, fs::Permissions::from_mode(0o755))
            .with_context(|| format!("Unable to set {} rootless wrapper permissions", runtime))?;
    }
    Ok(())
}

/// Maximum usable bytes in a Unix socket path (108 - 1 for null terminator).
pub const MAX_SOCKET_PATH_LEN: usize = 107;

/// Simple CRI socket abstraction.
pub struct CriSocket(PathBuf);

impl Display for CriSocket {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0.display())
    }
}

impl CriSocket {
    /// Create a new CRI socket, validating the path length against Unix limits.
    pub fn new(path: PathBuf) -> Result<CriSocket> {
        if path.as_os_str().len() > MAX_SOCKET_PATH_LEN {
            bail!("Socket path '{}' is too long", path.display())
        }
        Ok(CriSocket(path))
    }

    /// Format the socket path as a `unix://` URI for CRI clients.
    pub fn to_socket_string(&self) -> String {
        format!("unix://{}", self.0.display())
    }
}

/// Return the CRI socket for the configured runtime and node.
pub fn cri_socket(config: &Config, network: &Network, node: u8) -> Result<CriSocket> {
    match config.cri_runtime() {
        CriRuntime::Crio => Crio::socket(config, network, node),
        CriRuntime::Containerd => Containerd::socket(config, network, node),
    }
}

/// The NRI socket and plugin directories of a node, below its working
/// directory, so that neither the socket nor the plugins of the host apply.
pub struct NriPaths {
    pub socket: PathBuf,
    pub plugin_dir: PathBuf,
    pub plugin_config_dir: PathBuf,
}

impl NriPaths {
    /// The NRI paths for the node working directory `dir`, validating the
    /// socket path length against Unix limits.
    pub fn new(dir: &Path) -> Result<Self> {
        let socket = dir.join("nri.sock");
        if socket.as_os_str().len() > MAX_SOCKET_PATH_LEN {
            bail!("NRI socket path '{}' is too long", socket.display())
        }
        let nri = dir.join("nri");
        Ok(Self {
            socket,
            plugin_dir: nri.join("plugins"),
            plugin_config_dir: nri.join("conf.d"),
        })
    }

    /// The plugin directories, which get created if NRI is enabled.
    pub fn dirs(&self) -> [&Path; 2] {
        [&self.plugin_dir, &self.plugin_config_dir]
    }
}

/// Directory used by the host-local IPAM plugin to store the allocated IPs,
/// instead of the default `/var/lib/cni/networks`, which is not writable in
/// rootless mode. CNI skips directories when loading the network configs.
fn ipam_data_dir(cni_conf_dir: &Path) -> PathBuf {
    cni_conf_dir.join("networks")
}

/// Remove the IP allocations of the host-local IPAM plugin before the CRI
/// runtime starts. A clean stop releases them when removing the pods, but
/// after a force quit nothing else would. The allocations of a former run
/// are stale in any case: no pod of the node can run while the run root is
/// locked and the CRI runtime is not started, and the bridges of a former
/// run get removed on startup (see `Network::setup_bridges`), which
/// detaches the network of a container that may have survived a force
/// quit. Every rootless run gets a new network namespace anyway.
pub fn reset_ipam_data(cni_conf_dir: &Path) -> Result<()> {
    let dir = ipam_data_dir(cni_conf_dir);
    if dir.exists() {
        fs::remove_dir_all(&dir)
            .with_context(|| format!("Unable to remove IPAM data '{}'", dir.display()))?;
    }
    Ok(())
}

/// Write the CNI bridge network configuration for a node.
///
/// In root mode, kubernix creates the bridge with the gateway address
/// before (see `Network::setup_bridges`), which the plugin keeps. The
/// plugin checks the IPv4 addresses of the bridge in the order the kernel
/// lists them until it finds the gateway, and fails on any other address
/// before it. Addresses added later with the same scope come after the
/// gateway, but the kernel lists addresses with a narrower scope first,
/// like link-local ones (`scope link`) of an IPv4LL client, and all others
/// come first once the gateway got removed. With `forceAddress`, the plugin
/// removes these addresses instead of failing to set up the pods. With
/// `portmap`, the portmap plugin maps the host ports of the pods, which
/// containerd passes as capability. CRI-O maps them itself, so the plugin
/// would map them twice.
pub fn write_cni_config(
    cni_conf_dir: &Path,
    node_name: &str,
    node: u8,
    network: &Network,
    portmap: bool,
) -> Result<()> {
    let cidr = network
        .pod_cidrs()
        .get(node as usize)
        .with_context(|| format!("Unable to find CIDR for {}", node_name))?;
    let mut plugins = vec![json!({
        "type": "bridge",
        "bridge": network.bridge(node),
        "isGateway": true,
        "forceAddress": true,
        "ipMasq": true,
        "hairpinMode": true,
        "ipam": {
            "type": "host-local",
            "dataDir": ipam_data_dir(cni_conf_dir),
            "routes": [{ "dst": "0.0.0.0/0" }],
            "ranges": [[{ "subnet": cidr }]]
        }
    })];
    if portmap {
        plugins.push(json!({
            "type": "portmap",
            "capabilities": { "portMappings": true }
        }));
    }
    // The firewall plugin accepts the pod traffic in the FORWARD chain,
    // which may have a DROP policy, for example when Docker is running.
    plugins.push(json!({ "type": "firewall" }));
    write_file(
        &cni_conf_dir.join("10-bridge.conflist"),
        to_string_pretty(&json!({
            "cniVersion": "1.0.0",
            "name": format!("kubernix-{}", node_name),
            "plugins": plugins
        }))?
        .as_bytes(),
    )
}

/// Timeout of the crictl requests. The CRI runtime stops the containers of
/// a removed pod within the deadline of the request, which defaults to two
/// seconds in crictl, while the kubelet grants 30 seconds by default.
const CRICTL_TIMEOUT: &str = "--timeout=30s";

/// How often removing a pod gets tried.
const RMP_ATTEMPTS: usize = 3;

/// Delay between the attempts to remove a pod.
const RMP_RETRY_DELAY: Duration = if cfg!(test) {
    Duration::from_millis(10)
} else {
    Duration::from_secs(1)
};

/// Executables which monitor the containers of a pod. They run in their own
/// process groups and outlive the CRI runtime if removing a pod failed.
const MONITORS: &[&str] = &["conmonrs", "conmon", "containerd-shim-runc-v2"];

/// Kill the container monitors of the node which are left after the CRI
/// runtime stopped, found by the node directory in their command line.
pub fn kill_leftover_monitors(socket: &CriSocket, node_name: &str) {
    let Some(dir) = socket.0.parent().and_then(Path::to_str) else {
        return;
    };
    let leftovers = process::find_processes_containing(MONITORS, dir);
    if !leftovers.is_empty() {
        warn!(
            "Killing {} leftover container monitor processes on {}",
            leftovers.len(),
            node_name
        );
        process::signal_processes(&leftovers, Signal::SIGKILL);
    }
}

/// Remove all containers and pods via crictl.
pub fn remove_all_containers(label: &str, socket: &CriSocket, node_name: &str) -> Result<()> {
    debug!("Removing all {} workloads on {}", label, node_name);
    remove_all_pods(OsStr::new("crictl"), socket, node_name)?;
    debug!("All workloads removed on {}", node_name);
    Ok(())
}

/// Remove all pods via the crictl executable `crictl`. Every pod gets
/// removed, even if removing another one failed, since the monitors of
/// their containers run in their own process groups and would be leaked.
fn remove_all_pods(crictl: &OsStr, socket: &CriSocket, node_name: &str) -> Result<()> {
    let command = |args: &[&str]| {
        // An interrupt in the terminal must not abort the removal
        let mut command = process::command(crictl);
        command
            .env(RUNTIME_ENV, socket.to_socket_string())
            .arg(CRICTL_TIMEOUT)
            .args(args);
        command
    };

    let output = command(&["pods", "-q"])
        .output()
        .context("Unable to run crictl")?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    if !output.status.success() {
        debug!("crictl pods stdout ({}): {}", node_name, stdout);
        debug!(
            "crictl pods stderr ({}): {}",
            node_name,
            String::from_utf8_lossy(&output.stderr)
        );
        bail!("crictl pods command failed ({})", node_name);
    }

    let pods: Vec<&str> = stdout.lines().filter(|x| !x.is_empty()).collect();
    let mut failed = pods.clone();
    for attempt in 1..=RMP_ATTEMPTS {
        if attempt > 1 {
            // The runtime may still process a request of the stopped
            // kubelet for the same pod, which makes the removal fail once
            thread::sleep(RMP_RETRY_DELAY);
        }
        let pending = std::mem::take(&mut failed);
        for pod in pending {
            debug!(
                "Removing pod {} on {} (attempt {})",
                pod, node_name, attempt
            );
            match command(&["rmp", "-f", pod]).output() {
                Ok(output) if output.status.success() => {}
                Ok(output) => {
                    debug!("crictl rmp {} ({}): {:?}", pod, node_name, output);
                    failed.push(pod);
                }
                Err(e) => {
                    debug!("crictl rmp {} ({}): {}", pod, node_name, e);
                    failed.push(pod);
                }
            }
        }
        if failed.is_empty() {
            break;
        }
    }
    if !failed.is_empty() {
        bail!(
            "Unable to remove {} of {} pods ({}): {}",
            failed.len(),
            pods.len(),
            node_name,
            failed.join(", ")
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    fn find_only(available: OciRuntime) -> impl Fn(OciRuntime) -> Result<String> {
        move |r| {
            if r == available {
                Ok(format!("/bin/{}", r))
            } else {
                bail!("{} not found", r)
            }
        }
    }

    #[test]
    fn available_runtimes_all() -> Result<()> {
        let runtimes =
            RuntimePaths::available_runtimes(OciRuntime::Crun, |r| Ok(format!("/bin/{}", r)))?;
        assert_eq!(
            runtimes,
            vec![
                (OciRuntime::Crun, "/bin/crun".to_string()),
                (OciRuntime::Runc, "/bin/runc".to_string()),
            ]
        );
        Ok(())
    }

    #[test]
    fn available_runtimes_skips_missing_non_default() -> Result<()> {
        let runtimes =
            RuntimePaths::available_runtimes(OciRuntime::Crun, find_only(OciRuntime::Crun))?;
        assert_eq!(runtimes, vec![(OciRuntime::Crun, "/bin/crun".to_string())]);
        Ok(())
    }

    #[test]
    fn available_runtimes_fails_on_missing_default() {
        let err = RuntimePaths::available_runtimes(OciRuntime::Runc, find_only(OciRuntime::Crun))
            .unwrap_err();
        assert!(format!("{:#}", err).contains("default OCI runtime 'runc'"));
    }

    #[test]
    fn cri_socket_success() -> Result<()> {
        CriSocket::new("/some/path.sock".into())?;
        Ok(())
    }

    #[test]
    fn cri_socket_failure() {
        assert!(CriSocket::new("a".repeat(MAX_SOCKET_PATH_LEN + 1).into()).is_err());
    }

    #[test]
    fn nri_paths_below_node_dir() -> Result<()> {
        let nri = NriPaths::new(Path::new("/kx/crio/node"))?;
        assert_eq!(nri.socket, Path::new("/kx/crio/node/nri.sock"));
        assert_eq!(
            nri.dirs(),
            [
                Path::new("/kx/crio/node/nri/plugins"),
                Path::new("/kx/crio/node/nri/conf.d"),
            ]
        );
        Ok(())
    }

    #[test]
    fn nri_paths_socket_too_long() -> Result<()> {
        // The longest directory whose socket still fits
        let fits = "a".repeat(MAX_SOCKET_PATH_LEN - "/nri.sock".len());
        NriPaths::new(Path::new(&fits))?;
        let err = NriPaths::new(Path::new(&format!("{}a", fits)))
            .err()
            .context("expected error")?
            .to_string();
        assert!(err.contains("NRI socket path"), "{}", err);
        Ok(())
    }

    #[test]
    fn cri_socket_string_format() -> Result<()> {
        let socket = CriSocket::new("/run/crio.sock".into())?;
        assert_eq!(socket.to_socket_string(), "unix:///run/crio.sock");
        assert_eq!(socket.to_string(), "/run/crio.sock");
        Ok(())
    }

    #[test]
    fn write_rootless_wrappers_success() -> Result<()> {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir()?;
        let paths = RuntimePaths {
            runtimes: vec![
                (OciRuntime::Crun, "/nix/bin/crun".into()),
                (OciRuntime::Runc, "/nix/bin/runc".into()),
            ],
            plugin_dir: "/nix/cni".into(),
        };
        // Writing them twice keeps them unchanged
        for _ in 0..2 {
            write_rootless_wrappers_with(dir.path(), &paths, "/nix/bin/jq")?;
        }
        for (runtime, path) in &paths.runtimes {
            let wrapper = rootless_wrapper(dir.path(), *runtime);
            assert_eq!(wrapper, dir.path().join(format!("{}-rootless", runtime)));
            let script = std::fs::read_to_string(&wrapper)?;
            assert!(script.starts_with("#!/bin/sh\n"));
            assert!(script.contains("'/nix/bin/jq' --argjson"));
            assert!(script.ends_with(&format!("exec '{}' \"$@\"\n", path)));
            assert!(!script.contains("__"));
            let mode = std::fs::metadata(&wrapper)?.permissions().mode();
            assert_eq!(mode & 0o777, 0o755);
        }
        Ok(())
    }

    #[test]
    fn handlers_use_wrappers_in_rootless_mode() -> Result<()> {
        let paths = RuntimePaths {
            runtimes: vec![
                (OciRuntime::Crun, "/nix/bin/crun".into()),
                (OciRuntime::Runc, "/nix/bin/runc".into()),
            ],
            plugin_dir: "/nix/cni".into(),
        };
        let dir = Path::new("/kubernix/crio/node");
        let mut config = crate::config::tests::parse(&[])?;
        config.set_rootless(false);
        assert_eq!(paths.handlers(&config, dir), paths.runtimes);
        config.set_rootless(true);
        assert_eq!(
            paths.handlers(&config, dir),
            vec![
                (OciRuntime::Crun, "/kubernix/crio/node/crun-rootless".into()),
                (OciRuntime::Runc, "/kubernix/crio/node/runc-rootless".into()),
            ]
        );
        Ok(())
    }

    /// Write an executable script.
    fn write_script(path: &Path, content: &str) -> Result<()> {
        use std::os::unix::fs::PermissionsExt;
        fs::write(path, content)?;
        fs::set_permissions(path, fs::Permissions::from_mode(0o755))?;
        Ok(())
    }

    /// Run a command, retrying while another test thread holds the freshly
    /// written executable open for writing during a fork.
    fn output(command: &mut Command) -> Result<std::process::Output> {
        for _ in 0..50 {
            match command.output() {
                Err(e) if e.raw_os_error() == Some(nix::libc::ETXTBSY) => {
                    std::thread::sleep(std::time::Duration::from_millis(20))
                }
                result => return Ok(result?),
            }
        }
        bail!("Executable stays busy")
    }

    /// A rootless wrapper around a fake runtime, which records its arguments
    /// in `<dir>/runtime.args`.
    struct Wrapper {
        dir: tempfile::TempDir,
    }

    impl Wrapper {
        /// Create the wrapper, `None` if jq is not available.
        fn new() -> Result<Option<Self>> {
            let Ok(jq) = System::find_executable("jq") else {
                eprintln!("Skipping the rootless wrapper test, jq not found");
                return Ok(None);
            };
            let dir = tempfile::tempdir()?;
            let runtime = dir.path().join("runtime");
            write_script(&runtime, "#!/bin/sh\nprintf '%s\\n' \"$@\" > \"$0.args\"\n")?;
            let paths = RuntimePaths {
                runtimes: vec![(OciRuntime::Crun, runtime.display().to_string())],
                plugin_dir: "/nix/cni".into(),
            };
            write_rootless_wrappers_with(dir.path(), &paths, &jq.display().to_string())?;
            fs::create_dir(dir.path().join("bundle"))?;
            Ok(Some(Self { dir }))
        }

        fn bundle(&self) -> PathBuf {
            self.dir.path().join("bundle")
        }

        /// Run the wrapper with the arguments.
        fn run(&self, args: &[&str]) -> Result<std::process::Output> {
            output(Command::new(rootless_wrapper(self.dir.path(), OciRuntime::Crun)).args(args))
        }

        /// Run `create` on a bundle with the config, and return the patched
        /// config.
        fn create(&self, config: &serde_json::Value) -> Result<serde_json::Value> {
            let bundle = self.bundle();
            fs::write(bundle.join("config.json"), config.to_string())?;
            let bundle = bundle.display().to_string();
            let output = self.run(&["create", "--bundle", &bundle, "id"])?;
            assert!(output.status.success(), "{:?}", output);
            assert_eq!(
                self.runtime_args()?,
                Some(format!("create\n--bundle\n{}\nid\n", bundle))
            );
            Ok(serde_json::from_str(&fs::read_to_string(
                self.bundle().join("config.json"),
            )?)?)
        }

        fn runtime_args(&self) -> Result<Option<String>> {
            let file = self.dir.path().join("runtime.args");
            Ok(file
                .exists()
                .then(|| fs::read_to_string(file))
                .transpose()?)
        }
    }

    /// The OOM score of this process, which the wrapper inherits.
    fn own_oom_score_adj() -> Result<i64> {
        Ok(fs::read_to_string("/proc/self/oom_score_adj")?
            .trim()
            .parse()?)
    }

    #[test]
    fn rootless_wrapper_clamps_oom_score_adj() -> Result<()> {
        let Some(wrapper) = Wrapper::new()? else {
            return Ok(());
        };
        let own = own_oom_score_adj()?;
        for (score, expected) in [(-998, own.max(-998)), (own, own), (1000, 1000)] {
            let config = wrapper.create(&json!({"process": {"oomScoreAdj": score}}))?;
            assert_eq!(config["process"]["oomScoreAdj"], expected);
        }
        let config = wrapper.create(&json!({"process": {}}))?;
        assert!(config["process"].get("oomScoreAdj").is_none());
        Ok(())
    }

    #[test]
    fn rootless_wrapper_prunes_own_namespaces() -> Result<()> {
        let Some(wrapper) = Wrapper::new()? else {
            return Ok(());
        };
        // The wrapper runs in the namespaces of this process
        let own = |ns: &str| format!("/proc/{}/ns/{}", std::process::id(), ns);
        let config = wrapper.create(&json!({"linux": {"namespaces": [
            {"type": "pid"},
            {"type": "network", "path": own("net")},
            {"type": "uts", "path": own("uts")},
            {"type": "ipc", "path": "/nonexistent"},
            {"type": "mount"},
        ]}}))?;
        assert_eq!(
            config["linux"]["namespaces"],
            json!([
                {"type": "uts"},
                {"type": "pid"},
                {"type": "ipc", "path": "/nonexistent"},
                {"type": "mount"},
            ])
        );
        Ok(())
    }

    #[test]
    fn rootless_wrapper_keeps_uts_namespace() -> Result<()> {
        let Some(wrapper) = Wrapper::new()? else {
            return Ok(());
        };
        let namespaces = json!([{"type": "uts", "path": "/nonexistent"}, {"type": "pid"}]);
        let config = wrapper.create(&json!({"linux": {"namespaces": namespaces}}))?;
        assert_eq!(config["linux"]["namespaces"], namespaces);
        let config = wrapper.create(&json!({}))?;
        assert_eq!(config["linux"]["namespaces"], json!([{"type": "uts"}]));
        Ok(())
    }

    #[test]
    fn rootless_wrapper_passes_other_commands() -> Result<()> {
        let Some(wrapper) = Wrapper::new()? else {
            return Ok(());
        };
        let output = wrapper.run(&["state", "id"])?;
        assert!(output.status.success(), "{:?}", output);
        assert_eq!(wrapper.runtime_args()?.as_deref(), Some("state\nid\n"));
        Ok(())
    }

    #[test]
    fn rootless_wrapper_fails_on_invalid_config() -> Result<()> {
        let Some(wrapper) = Wrapper::new()? else {
            return Ok(());
        };
        let bundle = wrapper.bundle();
        fs::write(bundle.join("config.json"), "{")?;
        let arg = format!("--bundle={}", bundle.display());
        let output = wrapper.run(&["create", &arg, "id"])?;
        assert!(!output.status.success());
        assert!(String::from_utf8_lossy(&output.stderr).contains("unable to patch"));
        assert_eq!(wrapper.runtime_args()?, None);
        assert_eq!(fs::read_to_string(bundle.join("config.json"))?, "{");
        assert!(!bundle.join("config.json.rootless").exists());
        Ok(())
    }

    #[test]
    fn remove_all_pods_tries_every_pod() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let crictl = dir.path().join("crictl");
        write_script(
            &crictl,
            "#!/bin/sh\n\
             echo \"$CONTAINER_RUNTIME_ENDPOINT $*\" >> \"$0.calls\"\n\
             case \"$*\" in\n\
             *' pods -q') printf 'a\\nb\\nc\\n' ;;\n\
             *' rmp -f b') exit 1 ;;\n\
             esac\n",
        )?;
        // Not bound, a fixed path fits into a unix socket path for any TMPDIR
        let socket = CriSocket::new("/kx/crio.sock".into())?;
        let err = loop {
            match remove_all_pods(crictl.as_os_str(), &socket, "node") {
                Err(e) if format!("{:#}", e).contains("busy") => {
                    std::thread::sleep(std::time::Duration::from_millis(20))
                }
                result => break result.unwrap_err(),
            }
        };
        assert_eq!(
            format!("{:#}", err),
            "Unable to remove 1 of 3 pods (node): b"
        );
        let endpoint = socket.to_socket_string();
        let calls: Vec<String> = [
            "pods -q", "rmp -f a", "rmp -f b", "rmp -f c", "rmp -f b", "rmp -f b",
        ]
        .iter()
        .map(|args| format!("{} --timeout=30s {}\n", endpoint, args))
        .collect();
        assert_eq!(
            fs::read_to_string(dir.path().join("crictl.calls"))?,
            calls.concat()
        );
        Ok(())
    }

    #[test]
    fn remove_all_pods_retries_failed_pods() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let crictl = dir.path().join("crictl");
        // The first removal of b fails, the second one succeeds
        write_script(
            &crictl,
            "#!/bin/sh\n\
             case \"$*\" in\n\
             *' pods -q') printf 'a\\nb\\n' ;;\n\
             *' rmp -f b') [ -e \"$0.failed\" ] || { touch \"$0.failed\"; exit 1; } ;;\n\
             esac\n",
        )?;
        let socket = CriSocket::new("/kx/crio.sock".into())?;
        loop {
            match remove_all_pods(crictl.as_os_str(), &socket, "node") {
                Err(e) if format!("{:#}", e).contains("busy") => {
                    std::thread::sleep(std::time::Duration::from_millis(20))
                }
                result => break result?,
            }
        }
        assert!(dir.path().join("crictl.failed").exists());
        Ok(())
    }

    #[test]
    fn remove_all_pods_fails_without_runtime() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let crictl = dir.path().join("crictl");
        write_script(&crictl, "#!/bin/sh\nexit 1\n")?;
        // Not bound, a fixed path fits into a unix socket path for any TMPDIR
        let socket = CriSocket::new("/kx/crio.sock".into())?;
        let err = loop {
            match remove_all_pods(crictl.as_os_str(), &socket, "node") {
                Err(e) if format!("{:#}", e).contains("busy") => {
                    std::thread::sleep(std::time::Duration::from_millis(20))
                }
                result => break result.unwrap_err(),
            }
        };
        assert!(format!("{:#}", err).contains("crictl pods command failed"));
        Ok(())
    }

    #[test]
    fn reset_ipam_data_removes_allocations() -> Result<()> {
        let dir = tempfile::tempdir()?;
        reset_ipam_data(dir.path())?;
        let ipam = ipam_data_dir(dir.path()).join("kubernix");
        std::fs::create_dir_all(&ipam)?;
        std::fs::write(ipam.join("10.10.128.2"), "id")?;
        reset_ipam_data(dir.path())?;
        assert!(!ipam_data_dir(dir.path()).exists());
        assert!(dir.path().exists());
        Ok(())
    }

    #[test]
    fn bridge_cni_config_uses_bridge() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let network = crate::network::Network::new(&*crate::config::tests::test_config()?)?;
        write_cni_config(dir.path(), "node-0", 0, &network, false)?;
        let content = std::fs::read_to_string(dir.path().join("10-bridge.conflist"))?;
        let v: serde_json::Value = serde_json::from_str(&content)?;
        let bridge = &v["plugins"][0];
        assert_eq!(bridge["type"], "bridge");
        assert_eq!(bridge["bridge"].as_str(), Some(network.bridge(0).as_str()));
        assert_eq!(bridge["isGateway"], true);
        assert_eq!(bridge["forceAddress"], true);
        assert_eq!(v["plugins"][1]["type"], "firewall");
        assert_eq!(v["plugins"].as_array().map(Vec::len), Some(2));
        assert_eq!(
            bridge["ipam"]["dataDir"].as_str(),
            Some(dir.path().join("networks").to_str().unwrap())
        );
        Ok(())
    }

    #[test]
    fn bridge_cni_config_with_portmap() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let network = crate::network::Network::new(&*crate::config::tests::test_config()?)?;
        write_cni_config(dir.path(), "node-0", 0, &network, true)?;
        let content = std::fs::read_to_string(dir.path().join("10-bridge.conflist"))?;
        let v: serde_json::Value = serde_json::from_str(&content)?;
        let types = v["plugins"]
            .as_array()
            .context("no plugins")?
            .iter()
            .map(|p| p["type"].as_str().unwrap_or_default())
            .collect::<Vec<_>>();
        assert_eq!(types, ["bridge", "portmap", "firewall"]);
        assert_eq!(v["plugins"][1]["capabilities"]["portMappings"], true);
        Ok(())
    }
}
