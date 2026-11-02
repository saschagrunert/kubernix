//! Kubernetes development cluster bootstrapping with Nix packages.
//!
//! This crate implements the `kubernix` binary, which bootstraps a local
//! Kubernetes cluster for testing and development. All components (etcd,
//! the Kubernetes binaries, CRI-O or containerd, crun and runc, the CNI
//! plugins, etc.) come from a pinned Nix environment, so Nix with flakes
//! enabled is the only real host dependency.
//!
//! The library target only exists to share the implementation with the
//! binary and is not a public API: its items are hidden and may change in
//! any release. Semantic versioning covers the command line interface and
//! the other interfaces listed in the "Stability" section of the
//! [README](https://github.com/saschagrunert/kubernix#stability).
//!
//! Internally, `Config` holds the command line options, `Logger` is the log
//! implementation and `Kubernix` runs the cluster based on the
//! configuration. `Kubernix::start` works in these steps:
//!
//! 1. Lock the run directory (`kubernix-run` by default), so that only one
//!    instance uses it. If not started as root, check the prerequisites of
//!    rootless mode on the host. Then write the configuration into the run
//!    directory, write a Nix flake into it and re-execute the binary inside
//!    `nix develop`, forwarding all options. The evaluated environment is
//!    cached as a Nix profile, which is re-entered as long as the flake
//!    files are unchanged.
//! 2. If not started as root, re-execute again inside `rootlesskit`, which
//!    provides a user namespace where all components run as fake root
//!    (rootless mode).
//! 3. Remove the leftovers of an unclean exit and check that the ports of
//!    the components are free. Prepare the system: load the required
//!    kernel modules (root mode only) and set the network sysctls, which
//!    root mode skips inside a container and restores once the last
//!    cluster in root mode stops. Create the CNI bridges of the run
//!    directory, whose gateway addresses are the node IPs in root mode.
//!    With multiple nodes in root mode, prepare the node container image.
//!    Then generate the certificates, kubeconfigs and the encryption
//!    config, and start the components in boot phases: etcd, the API
//!    server, the controllers together with the CRI runtimes and
//!    kube-proxy, and finally the kubelets.
//! 4. Deploy the addons (CoreDNS by default) in the background, and spawn
//!    an interactive shell or wait for a termination signal with
//!    `--no-shell`. Stop all components when it exits.
//!
//! With more than one node, every node gets its own CRI runtime and kubelet.
//! In root mode, they run inside containers (podman by default), which
//! mount the Nix store of the host read-only, while in rootless mode they
//! run as processes inside the rootlesskit namespaces, every kubelet in its
//! own mount namespace. All nodes share one network namespace with a bridge
//! per node, and one iptables rule exempts the traffic between pods from the
//! masquerading of the bridges.
//!
//! The subcommands work on the cluster of an existing run directory:
//! `Kubernix::new_shell` spawns an additional shell (`kubernix shell`) or
//! runs a command in it (`kubernix shell -- <cmd>`), entering the
//! namespaces of the cluster via `nsenter` in rootless mode.
//! `Kubernix::stop_cluster` stops the running instance (`kubernix stop`)
//! and `Kubernix::status` prints its state (`kubernix status`).
//!
//! See the [README](https://github.com/saschagrunert/kubernix) for usage
//! details.
#![deny(missing_docs)]

mod apiserver;
mod component;
mod config;
mod container;
mod containerd;
mod controllermanager;
mod coredns;
mod cri;
mod crio;
mod encryptionconfig;
mod etcd;
mod files;
mod hoststate;
mod kubeconfig;
mod kubectl;
mod kubelet;
mod logger;
mod network;
mod nix;
mod node;
mod pki;
mod podman;
mod process;
mod progress;
mod proxy;
mod rootless;
mod runlock;
mod scheduler;
mod shell;
mod signals;
mod system;

// Only used by the binary, not a public API.
#[doc(hidden)]
pub use config::{Config, SubCommand};
#[doc(hidden)]
pub use logger::Logger;

pub(crate) use files::{write_file, write_if_changed, write_private};

/// A hash of the provided inputs, used to detect changed content. FNV-1a,
/// stable across Rust versions and platforms.
pub(crate) fn content_hash(inputs: &[&[u8]]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for input in inputs {
        for byte in (input.len() as u64).to_le_bytes().iter().chain(*input) {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(0x0100_0000_01b3);
        }
    }
    hash
}

use crate::nix::Nix;
use component::{ClusterContext, ComponentRegistry};
use config::{CriRuntime, OciRuntime, OutputFormat};
use container::Container;
use coredns::CoreDns;
use cri::RUNTIME_ENV;
use encryptionconfig::EncryptionConfig;
use kubeconfig::KubeConfig;
use kubectl::Kubectl;
use kubelet::Kubelet;
use network::{Bridges, NatExemption, Network};
use pki::Pki;
use process::ProcStat;
use process::Stoppables;
use progress::Progress;
use rootless::Rootless;
use runlock::RunLock;
use system::System;

/// The internal environment variable which marks the stage running inside
/// rootlesskit. Not part of the interface, unlike the `KUBERNIX_*` options.
const ROOTLESS_ENV: &str = "_KUBERNIX_ROOTLESS";

/// The internal environment variable which marks the shells of a rootless
/// cluster, with its run directory as value. Root in a user namespace runs
/// in rootless mode there, but in root mode otherwise, for example in a
/// rootless container.
const CLUSTER_SHELL_ENV: &str = "_KUBERNIX_CLUSTER_SHELL";

/// The environment variable of the `--root` option, which the shells of the
/// cluster get, so that the subcommands find it from any directory.
const ROOT_ENV: &str = "KUBERNIX_ROOT";

/// The lock file in the root, held by the instance running the cluster.
const LOCK_FILE: &str = "kubernix.lock";

/// The PID file in the root, written once the cluster is up.
const PID_FILE: &str = "kubernix.pid";

/// The file in the root which lists the process groups of the components,
/// so that the next start can kill the leftovers of an unclean exit.
const PROCESS_GROUPS_FILE: &str = "kubernix.pgroups";

/// The port the API server listens on.
pub(crate) const API_SERVER_PORT: u16 = 6443;

/// The exit code of `kubernix stop`, `kubernix status` and `kubernix shell`
/// if no cluster is running in the run root.
#[doc(hidden)]
pub const EXIT_NOT_RUNNING: i32 = 3;

/// The configuration file of a run root, as found by `prepare_env`.
enum ConfigFile {
    /// Not existing yet
    Missing,
    /// Existing and up to date
    Current,
    /// Existing, but has to be replaced by the migrated configuration
    Migrated(Box<Config>),
}

impl ConfigFile {
    fn exists(&self) -> bool {
        !matches!(self, ConfigFile::Missing)
    }
}

/// The state of the cluster in a run root.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum State {
    /// No kubernix instance uses the run root.
    Stopped,
    /// A kubernix instance bootstraps the cluster, with the PID of the
    /// process which took the lock if known.
    Starting(Option<i32>),
    /// The cluster is up, run by the kubernix instance with the PID.
    Running(i32),
}

impl State {
    fn name(self) -> &'static str {
        match self {
            State::Stopped => "stopped",
            State::Starting(_) => "starting",
            State::Running(_) => "running",
        }
    }

    fn pid(self) -> Option<i32> {
        match self {
            State::Stopped => None,
            State::Starting(pid) => pid,
            State::Running(pid) => Some(pid),
        }
    }
}

/// The status of the cluster in a run root, as shown by `kubernix status`.
/// The JSON fields use kebab-case like the options and `kubernix.toml`.
#[derive(Debug, Serialize)]
#[serde(rename_all = "kebab-case")]
struct Status {
    root: PathBuf,
    state: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pid: Option<i32>,
    #[serde(flatten, skip_serializing_if = "Option::is_none")]
    cluster: Option<ClusterStatus>,
}

/// The settings and nodes of an existing cluster.
#[derive(Debug, Serialize)]
#[serde(rename_all = "kebab-case")]
struct ClusterStatus {
    mode: &'static str,
    nodes: u8,
    cri_runtime: CriRuntime,
    oci_runtime: OciRuntime,
    api_server: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    node_status: Option<Vec<NodeStatus>>,
}

/// The readiness of a node.
#[derive(Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
struct NodeStatus {
    name: String,
    ready: bool,
}

impl NodeStatus {
    /// The kubectl output template for `parse`.
    const TEMPLATE: &'static str = concat!(
        "go-template={{range .items}}{{.metadata.name}}",
        "{{range .status.conditions}}{{if eq .type \"Ready\"}} {{.status}}{{end}}{{end}}",
        "{{\"\\n\"}}{{end}}",
    );

    /// Parse the kubectl output of `TEMPLATE`.
    fn parse(output: &str) -> Vec<NodeStatus> {
        output
            .lines()
            .filter_map(|line| {
                let mut fields = line.split_whitespace();
                Some(NodeStatus {
                    name: fields.next()?.into(),
                    ready: fields.next() == Some("True"),
                })
            })
            .collect()
    }
}

impl Status {
    /// The human-readable status.
    fn text(&self) -> String {
        let mut lines = vec![("Root", self.root.display().to_string())];
        lines.push((
            "State",
            match self.pid {
                Some(pid) => format!("{} (PID {})", self.state, pid),
                None => self.state.into(),
            },
        ));
        if let Some(c) = &self.cluster {
            lines.extend([
                ("Mode", c.mode.into()),
                ("Nodes", c.nodes.to_string()),
                ("CRI runtime", c.cri_runtime.to_string()),
                ("OCI runtime", c.oci_runtime.to_string()),
                ("API server", c.api_server.clone()),
            ]);
        }
        let mut out: String = lines
            .iter()
            .map(|(k, v)| format!("{:<12} {}\n", format!("{}:", k), v))
            .collect();
        for node in self
            .cluster
            .iter()
            .flat_map(|c| c.node_status.iter().flatten())
        {
            let ready = if node.ready { "Ready" } else { "NotReady" };
            out.push_str(&format!("{:<12} {} {}\n", "Node:", node.name, ready));
        }
        out
    }
}

use ::nix::{
    errno::Errno,
    mount::{MntFlags, umount2},
    sys::signal::{Signal, kill},
    unistd::{Pid, getuid},
};
use anyhow::{Context, Result, bail};
use log::{debug, error, info, warn};
use serde::Serialize;
use std::{
    ffi::OsString,
    fs,
    io::{BufRead, BufReader, ErrorKind},
    net::Ipv4Addr,
    os::unix::{ffi::OsStringExt, process::CommandExt},
    path::{Path, PathBuf},
    process::{Child, Command, id},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU8, Ordering},
    },
    thread::{self, sleep},
    time::{Duration, Instant},
};

/// The main entry point for the application
#[doc(hidden)]
pub struct Kubernix {
    addon_shutdown: Arc<AtomicBool>,
    addon_thread: Option<thread::JoinHandle<()>>,
    config: Config,
    network: Network,
    kubectl: Kubectl,
    processes: Vec<Stoppables>,
    // Removes the rule on drop, after the processes are stopped
    nat_exemption: Option<NatExemption>,
    // Removes the CNI bridges on drop, after the processes are stopped
    bridges: Option<Bridges>,
    // Restores the system state on drop, after the bridges are removed
    system: Option<System>,
    // Dropped after the cleanup, releases the run directory
    _lock: RunLock,
}

impl Kubernix {
    /// Start kubernix by consuming the provided configuration.
    ///
    /// Re-executes the binary inside the Nix environment (and inside
    /// rootlesskit in rootless mode) if necessary, bootstraps the cluster
    /// and blocks until the shell exits or a termination signal arrives.
    pub fn start(mut config: Config) -> Result<()> {
        let file = Self::prepare_env(&mut config, true)?;

        // Fail early if another instance runs on the same root. The next
        // stages inherit the lock. Only its holder writes the configuration.
        let lock = Self::lock(&config)?;

        // The rootless prerequisites only need the tools of the host. Check
        // them before writing the configuration, otherwise a new run
        // directory would stay a rootless one if they are not met.
        if config.is_rootless() && !Nix::is_active() {
            Rootless::preflight(API_SERVER_PORT)?;
        }
        let new = Self::write_config(&config, file)?;

        // Bootstrap if we're not inside a nix shell
        if Nix::is_active() {
            if config.is_rootless() && std::env::var(ROOTLESS_ENV).as_deref() != Ok("1") {
                Network::warn_overlapping_route(config.cidr(), "cluster CIDR", None)?;
                Network::warn_overlapping_route(
                    config::SLIRP4NETNS_CIDR,
                    "slirp4netns network",
                    None,
                )?;
                return Self::reexec_rootless(&lock);
            }
            Self::bootstrap_cluster(config, lock)
        } else {
            // Abort the evaluation of the Nix environment on termination
            // signals, for example of 'kubernix stop', which removes the
            // configuration of a new run directory below.
            signals::register()?;

            // A new run directory keeps its configuration only if the Nix
            // environment works, which fails for example for packages
            // which do not exist.
            let file = config.file();
            let result = Nix::bootstrap(config, &lock);
            if new
                && result.is_err()
                && let Err(e) = fs::remove_file(&file)
            {
                warn!("Unable to remove '{}': {}", file.display(), e);
            }
            result
        }
    }

    /// Spawn a new shell into the provided configuration environment, or
    /// run the command of the `shell` subcommand in it. The process gets
    /// replaced, so the exit code is the one of the shell or command.
    /// Returns false if no cluster is running.
    pub fn new_shell(mut config: Config) -> Result<bool> {
        let command = match config.subcommand() {
            Some(SubCommand::Shell { command }) => command.clone(),
            _ => vec![],
        };
        if !Self::prepare_env(&mut config, false)?.exists() {
            error!("No kubernix cluster found in '{}'", config.root().display());
            return Ok(false);
        }
        if Self::state(&config)? == State::Stopped {
            error!(
                "No running kubernix cluster found in '{}'",
                config.root().display()
            );
            return Ok(false);
        }
        let pid = Self::running_pid(&config)?;

        if command.is_empty() {
            info!(
                "Spawning new kubernix shell in: '{}'",
                config.root().display()
            );
        } else {
            debug!(
                "Running {:?} in kubernix shell: '{}'",
                command,
                config.root().display()
            );
        }

        let env_file = Self::env_file(&config);
        if !env_file.exists() {
            bail!(
                "Necessary environment file '{}' does not exist",
                env_file.display()
            )
        }

        // nsenter changes into the root directory of the mount namespace,
        // so keep the current one for relative paths
        let cwd = std::env::current_dir().ok();
        let namespaces = Self::shell_namespaces(&config, pid);
        let args = Self::shell_args(
            &config,
            pid,
            &namespaces,
            &env_file,
            cwd.as_deref(),
            &command,
        )?;
        Nix::exec(
            &config,
            &args.iter().map(String::as_str).collect::<Vec<_>>(),
        )
        .map(|()| true)
    }

    /// The `nsenter` options of the namespaces which the shell has to
    /// enter: in rootless mode the user, mount and network namespace of
    /// the rootlesskit session running the cluster (`pid`). The ones which
    /// are already shared get skipped, for example in the shell of the
    /// cluster, since entering the own user namespace fails.
    fn shell_namespaces(config: &Config, pid: i32) -> Vec<&'static str> {
        if !config.is_rootless() || std::env::var(ROOTLESS_ENV).as_deref() == Ok("1") {
            return vec![];
        }
        let ns = |pid: &str, name: &str| fs::read_link(format!("/proc/{}/ns/{}", pid, name)).ok();
        [("user", "-U"), ("mnt", "-m"), ("net", "-n")]
            .into_iter()
            .filter(|(name, _)| {
                let own = ns("self", name);
                own.is_none() || own != ns(&pid.to_string(), name)
            })
            .map(|(_, option)| option)
            .collect()
    }

    /// The arguments to run the shell, or the command if not empty, with
    /// the environment of the cluster. They enter the `namespaces` (see
    /// `shell_namespaces`) of `pid` and change into `cwd` afterwards.
    fn shell_args(
        config: &Config,
        pid: i32,
        namespaces: &[&str],
        env_file: &Path,
        cwd: Option<&Path>,
        command: &[String],
    ) -> Result<Vec<String>> {
        let mut args: Vec<String> = vec![];
        if !namespaces.is_empty() {
            args.extend(["nsenter".into(), "-t".into(), pid.to_string()]);
            args.extend(namespaces.iter().map(|x| x.to_string()));
            if namespaces.contains(&"-U") {
                args.push("--preserve-credentials".into());
            }
            // Opened before entering the namespaces, so it works for any
            // directory of the host
            if let Some(cwd) = cwd {
                args.push(format!("--wd={}", cwd.display()));
            }
        }
        let source = format!(
            ". {} && {}",
            shell::quote(&env_file.display().to_string()),
            Self::exports(&Self::shell_env(config)),
        );
        args.extend(["bash".into(), "-c".into()]);
        if command.is_empty() {
            args.push(format!(
                "{} && exec {}",
                source,
                shell::quote(&config.shell_ok()?)
            ));
        } else {
            // Pass the command as positional parameters, which avoids any
            // further quoting.
            args.extend([format!("{} && exec \"$@\"", source), "kubernix".into()]);
            args.extend(command.iter().cloned());
        }
        Ok(args)
    }

    /// The variables which the shells of the cluster get in addition to the
    /// env file: the run directory, so that the subcommands find the cluster
    /// from any directory, and the marker of the shells of a rootless
    /// cluster.
    fn shell_env(config: &Config) -> Vec<(&'static str, String)> {
        let root = config.root().display().to_string();
        let mut env = vec![(ROOT_ENV, root.clone())];
        if config.is_rootless() {
            env.push((CLUSTER_SHELL_ENV, root));
        }
        env
    }

    /// The shell commands which export the variables.
    fn exports(vars: &[(&str, String)]) -> String {
        vars.iter()
            .map(|(key, value)| format!("export {}={}", key, shell::quote(value)))
            .collect::<Vec<_>>()
            .join(" && ")
    }

    /// Stop the cluster running in the root of the configuration by
    /// sending SIGTERM to its kubernix instance and waiting for the
    /// cleanup to finish. A starting cluster aborts its bootstrap. Returns
    /// false if no cluster is running.
    pub fn stop_cluster(mut config: Config) -> Result<bool> {
        let timeout = match config.subcommand() {
            Some(SubCommand::Stop { timeout }) => *timeout,
            _ => 180,
        };
        // Run from the shell of the cluster, the cleanup sends SIGHUP to the
        // shell, which forwards it to its jobs. Keep waiting for the cleanup
        // to report its result.
        if let Err(e) = signals::survive_hangup() {
            debug!("Unable to handle SIGHUP: {:#}", e);
        }
        if !Self::prepare_env(&mut config, false)?.exists() {
            info!("No kubernix cluster found in '{}'", config.root().display());
            return Ok(false);
        }
        let root = config.root().display();

        let state = Self::state(&config)?;
        let pid = match state {
            State::Stopped => {
                info!("No running kubernix cluster found in '{}'", root);
                Self::remove_stale(&config);
                return Ok(false);
            }
            State::Starting(None) => bail!(
                "The kubernix cluster in '{}' is starting and its PID is unknown, \
                 retry once it is up",
                root
            ),
            State::Starting(Some(pid)) | State::Running(pid) => pid,
        };
        // The process which took the lock: the same one in root mode, and
        // rootlesskit in rootless mode, which keeps the API server port
        // published until it exits.
        let holder = RunLock::holder(&Self::lock_file(&config))?.flatten();
        // The processes with their start times, which tell apart a PID
        // which gets reused while waiting, without the exited ones.
        let started = |pid| ProcStat::read(pid).map(|s| (pid, s.start_time));
        let processes: Vec<_> = [Some(pid), holder]
            .into_iter()
            .flatten()
            .filter_map(started)
            .collect();

        info!(
            "Stopping {} kubernix (PID {}) in '{}'",
            state.name(),
            pid,
            root
        );
        match kill(Pid::from_raw(pid), Signal::SIGTERM) {
            Ok(()) | Err(Errno::ESRCH) => {}
            Err(Errno::EPERM) => bail!(
                "Not permitted to stop kubernix (PID {}) in '{}', use '{}' instead",
                pid,
                root,
                Config::sudo_command_line(),
            ),
            Err(e) => {
                return Err(e).with_context(|| format!("Unable to stop kubernix (PID {})", pid));
            }
        }

        let deadline = Instant::now() + Duration::from_secs(timeout);
        while RunLock::holder(&Self::lock_file(&config))?.is_some()
            || processes
                .iter()
                .any(|(pid, start)| ProcStat::alive_since(*pid, *start))
        {
            if Instant::now() >= deadline {
                bail!(
                    "kubernix (PID {}) did not finish the cleanup within {}s, \
                     run 'kubernix stop' again to force quit it",
                    pid,
                    timeout
                )
            }
            sleep(Duration::from_millis(200));
        }
        info!("Stopped kubernix cluster in '{}'", root);
        Ok(true)
    }

    /// Remove the node containers and mounts of a stopped cluster in root
    /// mode, which a killed kubernix instance leaves behind. It holds the
    /// lock of the run directory, so that it does not remove the ones of an
    /// instance which starts in the meantime.
    fn remove_stale(config: &Config) {
        if config.is_rootless() || !getuid().is_root() {
            return;
        }
        match RunLock::acquire(&Self::lock_file(config)) {
            Ok(Some(_lock)) => {
                Self::remove_stale_containers(config);
                Self::umount_below(config.root(), Duration::ZERO);
            }
            Ok(None) => debug!("Another kubernix instance started, skipping the stale cleanup"),
            Err(e) => warn!("Unable to lock the run directory: {:#}", e),
        }
    }

    /// Remove the node containers of a stopped multi-node cluster. The
    /// container runtime comes from the cached Nix environment, since
    /// `kubernix stop` runs outside of it.
    fn remove_stale_containers(config: &Config) {
        if !config.multi_node() {
            return;
        }
        let host_path = std::env::var_os("PATH").unwrap_or_default();
        let path = match Nix::cached_path(config.root()) {
            Some(nix_path) if !Nix::is_active() => {
                let mut path = OsString::from(nix_path);
                path.push(":");
                path.push(host_path);
                path
            }
            _ => host_path,
        };
        if let Err(e) = Container::remove_stale(config, Some(&path)) {
            debug!("Unable to remove stale node containers: {:#}", e);
        }
    }

    /// Print the status of the cluster in the root of the configuration.
    /// Returns false if no cluster is running.
    pub fn status(mut config: Config) -> Result<bool> {
        let output = match config.subcommand() {
            Some(SubCommand::Status { output }) => *output,
            _ => OutputFormat::Text,
        };
        let found = Self::prepare_env(&mut config, false)?.exists();
        let state = if found {
            Self::state(&config)?
        } else {
            State::Stopped
        };

        let cluster = found.then(|| ClusterStatus {
            mode: if config.is_rootless() {
                "rootless"
            } else {
                "root"
            },
            nodes: config.nodes(),
            cri_runtime: config.cri_runtime(),
            oci_runtime: config.oci_runtime(),
            api_server: format!("https://{}:{}", Ipv4Addr::LOCALHOST, API_SERVER_PORT),
            node_status: match state {
                State::Running(_) => Self::node_status(&config)
                    .inspect_err(|e| warn!("Unable to retrieve the node status: {:#}", e))
                    .ok(),
                _ => None,
            },
        });
        let status = Status {
            root: config.root().to_path_buf(),
            state: state.name(),
            pid: state.pid(),
            cluster,
        };

        match output {
            OutputFormat::Text => print!("{}", status.text()),
            OutputFormat::Json => println!("{}", serde_json::to_string(&status)?),
        }
        Ok(state != State::Stopped)
    }

    /// Retrieve the readiness of the nodes via kubectl.
    fn node_status(config: &Config) -> Result<Vec<NodeStatus>> {
        let kubeconfig = config
            .root()
            .join(KubeConfig::DIR)
            .join("admin.kubeconfig")
            .display()
            .to_string();
        let output = Nix::command(
            config,
            &[
                "kubectl",
                "--kubeconfig",
                &kubeconfig,
                "--request-timeout=10s",
                "get",
                "nodes",
                "-o",
                NodeStatus::TEMPLATE,
            ],
        )?
        .output()
        .context("Unable to run kubectl")?;
        if !output.status.success() {
            bail!(
                "kubectl exited with {}: {}",
                output.status,
                String::from_utf8_lossy(&output.stderr).trim()
            )
        }
        Ok(NodeStatus::parse(&String::from_utf8_lossy(&output.stdout)))
    }

    /// Prepare the environment based on the provided config. A new run
    /// directory gets only created if `bootstrap` is set, its configuration
    /// file gets written by `write_config` once the run directory is locked.
    fn prepare_env(config: &mut Config, bootstrap: bool) -> Result<ConfigFile> {
        // Use the log settings of the command line and the environment for
        // the errors before the configuration file got loaded
        Logger::install(config.log_level(), config.log_format())?;

        let rootless = Self::rootless_mode(
            getuid().is_root(),
            System::in_user_namespace(),
            std::env::var(ROOTLESS_ENV).as_deref() == Ok("1"),
            std::env::var_os(CLUSTER_SHELL_ENV).is_some(),
            bootstrap,
        )?;

        // Set rootless before config loading so that it can be compared
        // with the mode of the existing run directory.
        config.set_rootless(rootless);

        // Prepare the configuration. Only the bootstrap validates it, the
        // subcommands use the existing cluster as it is.
        let file = if config.file_exists() {
            // Root refuses a rootless run directory because of its mode,
            // before it gets refused because of its ownership.
            config.check_stored_mode()?;
            config.canonicalize_root()?;
            let migrated = config.load_file()?;
            if bootstrap {
                config.validate()?;
            }
            match migrated {
                Some(migrated) => ConfigFile::Migrated(migrated.into()),
                None => ConfigFile::Current,
            }
        } else {
            if bootstrap {
                config.validate()?;
                config.canonicalize_root()?;
            }
            ConfigFile::Missing
        };

        // The stored log settings apply unless overridden
        Logger::install(config.log_level(), config.log_format())?;

        if bootstrap && config.is_rootless() && !Nix::is_active() {
            info!("Running in rootless mode");
        }

        Ok(file)
    }

    /// Decide whether kubernix runs in rootless mode, which it does as a
    /// regular user. Root in a user namespace (`user_namespace`) runs in
    /// rootless mode inside rootlesskit (`rootless_stage`) and in the shell
    /// of a rootless cluster (`cluster_shell`), where a new cluster cannot
    /// start (`bootstrap`): its system setup would run in the namespaces of
    /// the running cluster. Root in any other user namespace, like a
    /// rootless or user namespaced container, runs in root mode.
    fn rootless_mode(
        root: bool,
        user_namespace: bool,
        rootless_stage: bool,
        cluster_shell: bool,
        bootstrap: bool,
    ) -> Result<bool> {
        if !root {
            return Ok(true);
        }
        if !user_namespace {
            return Ok(false);
        }
        if rootless_stage {
            return Ok(true);
        }
        if cluster_shell {
            if bootstrap {
                bail!(
                    "kubernix cannot start a cluster from the shell of a rootless cluster, \
                     run it as a regular user outside of it"
                )
            }
            return Ok(true);
        }
        Ok(false)
    }

    /// Write the configuration file of a new or migrated run directory
    /// found by `prepare_env`, which requires the lock of the run directory.
    /// So two instances starting at the same time cannot overwrite the
    /// configuration of each other. Returns true if the file is new.
    fn write_config(config: &Config, file: ConfigFile) -> Result<bool> {
        match file {
            ConfigFile::Current => Ok(false),
            ConfigFile::Migrated(migrated) => {
                config.write_migrated(&migrated)?;
                Ok(false)
            }
            // Another instance started and stopped a cluster in the
            // meantime, whose settings would get lost.
            ConfigFile::Missing if config.file_exists() => bail!(
                "Another kubernix instance created a cluster in '{}' in the meantime, \
                 start again to use it",
                config.root().display()
            ),
            ConfigFile::Missing => {
                config.to_file()?;
                Ok(true)
            }
        }
    }

    /// The lock file of the root.
    fn lock_file(config: &Config) -> PathBuf {
        config.root().join(LOCK_FILE)
    }

    /// Take the exclusive lock of the run directory, or adopt the one
    /// of the previous stage, which fails if another instance uses it.
    fn lock(config: &Config) -> Result<RunLock> {
        let path = Self::lock_file(config);
        match RunLock::acquire(&path)? {
            Some(lock) => {
                // No other instance runs, so the PID file is a leftover of
                // an unclean exit.
                Self::remove_pid_file(config);
                Ok(lock)
            }
            None => {
                let pid = RunLock::holder(&path)
                    .ok()
                    .flatten()
                    .flatten()
                    .map(|pid| format!(" (PID {})", pid))
                    .unwrap_or_default();
                bail!(
                    "Another kubernix instance{} is already running in '{}', \
                     use 'kubernix shell' to access it or a different --root",
                    pid,
                    config.root().display(),
                )
            }
        }
    }

    /// Retrieve the state of the cluster in the root. A PID file is only
    /// considered while the lock is held, since stale ones get removed
    /// when taking the lock.
    fn state(config: &Config) -> Result<State> {
        let Some(holder) = RunLock::holder(&Self::lock_file(config))? else {
            return Ok(State::Stopped);
        };

        let pid_file = Self::pid_file(config);
        let pid: i32 = match fs::read_to_string(&pid_file) {
            // Not written completely yet, for example by an older version
            Ok(pid) if pid.trim().is_empty() => return Ok(State::Starting(holder)),
            Ok(pid) => pid
                .trim()
                .parse()
                .with_context(|| format!("Invalid PID file '{}'", pid_file.display()))?,
            Err(e) if e.kind() == ErrorKind::NotFound => return Ok(State::Starting(holder)),
            Err(e) => {
                return Err(e).with_context(|| format!("Unable to read '{}'", pid_file.display()));
            }
        };
        match kill(Pid::from_raw(pid), None) {
            Ok(()) | Err(Errno::EPERM) => Ok(State::Running(pid)),
            Err(_) => bail!(
                "The kubernix instance (PID {}) in '{}' is not running",
                pid,
                config.root().display()
            ),
        }
    }

    /// Retrieve the PID of the instance running the cluster, which fails
    /// if no cluster is running or it is not ready yet.
    fn running_pid(config: &Config) -> Result<i32> {
        let root = config.root().display();
        match Self::state(config)? {
            State::Stopped => bail!("No running kubernix cluster found in '{}'", root),
            State::Starting(_) => bail!("The kubernix cluster in '{}' is not ready yet", root),
            State::Running(pid) => Ok(pid),
        }
    }

    /// Shell snippet which moves all processes of the cgroup into a child
    /// cgroup and enables the controllers for the children, since cgroup
    /// v2's "no internal processes" rule forbids it otherwise. Controllers
    /// which are not delegated get reported by `System::setup`.
    fn cgroup_evacuate(cgroup: &str, child: &str) -> String {
        format!(
            concat!(
                "mkdir -p \"{cg}/{child}\"; ",
                "for p in $(cat \"{cg}/cgroup.procs\"); do ",
                "echo $p >\"{cg}/{child}/cgroup.procs\" 2>/dev/null; done; ",
                "for c in {controllers}; do ",
                "echo \"+$c\" >\"{cg}/cgroup.subtree_control\" 2>/dev/null; done; ",
            ),
            cg = cgroup,
            child = child,
            controllers = rootless::CGROUP_CONTROLLERS.join(" "),
        )
    }

    /// Re-exec kubernix inside rootlesskit for rootless operation.
    /// All child processes inherit the user namespace.
    fn reexec_rootless(lock: &RunLock) -> Result<()> {
        info!("Re-executing inside rootlesskit");

        let exe = std::env::current_exe().context("Unable to determine current executable")?;
        let args: Vec<String> = std::env::args().skip(1).collect();

        let kubernix_args = args
            .iter()
            .map(|a| shell::quote(a))
            .collect::<Vec<_>>()
            .join(" ");
        let kubernix_exe = shell::quote(&exe.display().to_string());

        // Inside rootlesskit's cgroup namespace, evacuate processes from the
        // namespace root into a child and enable controllers, then exec.
        let rootlesskit_cmd = format!(
            "mount -t cgroup2 none /sys/fs/cgroup 2>/dev/null; {}exec {} {}",
            Self::cgroup_evacuate("/sys/fs/cgroup", "kubernix"),
            kubernix_exe,
            kubernix_args,
        );

        // Evacuate all processes from the scope's root cgroup into a child.
        // This must happen outside rootlesskit's cgroup namespace.
        //
        // The user namespace needs to own its network namespace to set up
        // the pod network, so slirp4netns provides the connectivity and
        // only the API server port gets published to the host.
        //
        // rootlesskit forwards all signals to its child.
        let outer_cmd = format!(
            concat!(
                "CGRP=/sys/fs/cgroup$(sed -n 's/^0:://p' /proc/self/cgroup); ",
                "{evacuate}",
                "exec rootlesskit --net=slirp4netns --mtu=65520 --cidr={cidr} ",
                "--disable-host-loopback --cgroupns ",
                "--port-driver=builtin --publish=127.0.0.1:{port}:{port}/tcp ",
                "--copy-up=/etc --copy-up=/run --copy-up=/var/cache ",
                "--copy-up=/var/lib --copy-up=/var/log --copy-up=/var/run ",
                "bash -c {cmd}",
            ),
            evacuate = Self::cgroup_evacuate("$CGRP", "init"),
            cidr = config::SLIRP4NETNS_CIDR,
            port = API_SERVER_PORT,
            cmd = shell::quote(&rootlesskit_cmd),
        );

        let mut cmd = Command::new("systemd-run");
        cmd.args([
            "--user",
            "--quiet",
            "--scope",
            "-p",
            "Delegate=yes",
            "--",
            "bash",
            "-c",
            &outer_cmd,
        ]);
        // All options are passed as arguments
        Config::remove_option_env(&mut cmd);
        cmd.env(ROOTLESS_ENV, "1");
        lock.inherit(&mut cmd)?;

        // Replace the process, so that signals reach the session directly.
        // No Kubernix struct or other Drop-bearing resources exist yet,
        // only the logger from prepare_env and the lock, which gets
        // inherited.
        Err(cmd.exec()).with_context(|| {
            format!(
                "Unable to run systemd-run, which rootless mode needs together with \
                 a systemd user session, see {}",
                rootless::TROUBLESHOOTING
            )
        })
    }

    /// Stop kubernix by cleaning up all running processes. The processes
    /// of a phase get stopped in parallel.
    fn stop(&mut self) {
        for group in &mut self.processes {
            thread::scope(|s| {
                for process in group.iter_mut() {
                    s.spawn(move || {
                        if let Err(e) = process.stop() {
                            warn!("{:#}", e)
                        }
                    });
                }
            });
        }
    }

    /// The amount of processes to be run
    fn processes(config: &Config) -> u64 {
        5 + 2 * u64::from(config.nodes())
    }

    /// Bootstrap the whole cluster, which assumes to be inside a nix shell
    fn bootstrap_cluster(config: Config, lock: RunLock) -> Result<()> {
        // Abort into the regular cleanup on termination signals, the
        // components do not receive terminal signals. Another signal force
        // quits, also during the bootstrap.
        signals::register()?;
        Self::watch_force_quit(&config);

        // Remove leftovers of a previous unclean exit, taking the lock
        // already removed the PID file. In rootless mode, the CRI storage
        // can only be removed inside the user namespace, otherwise its
        // stale containers would be reused.
        process::track_in(&config.root().join(PROCESS_GROUPS_FILE));
        if config.is_rootless() {
            Self::cleanup_rootless_storage(&config);
        } else {
            Self::umount_below(config.root(), Duration::ZERO);
        }

        // Setup the progress bar
        const BASE_STEPS: u64 = 15;
        let addon_steps = config
            .addons()
            .iter()
            .filter(|a| **a == config::Addon::Coredns)
            .count() as u64;
        let steps = if config.multi_node() {
            u64::from(config.nodes()) * 2 + BASE_STEPS
        } else {
            BASE_STEPS
        } + addon_steps
            + Self::processes(&config);
        let p = Progress::new(steps, config.log_level(), config.log_format());
        info!("Bootstrapping cluster");

        // Setup the network and fail early on unusable socket paths or
        // ports which are in use, for example by the cluster of another
        // root, before changing the system. The kubelets in the node
        // containers of a killed instance keep listening on their ports.
        Container::remove_stale_on_start(&config);
        let network = Network::new(&config)?;
        for node in 0..config.nodes() {
            Kubelet::validate_paths(&config, &network, node)?;
        }
        network
            .check_ports(config.nodes())
            .map_err(|e| Container::explain_port_conflict(&config, e))?;

        // Ensure that the system is prepared
        let system = System::setup(&config).context("Unable to setup system")?;
        if let Ok(mut restore) = FORCE_QUIT_HOSTS.lock() {
            *restore = Some(Box::new(system.hosts_restore()));
        }
        // Removed again on drop, also if anything below fails
        let bridges = network.setup_bridges()?;
        signals::check()?;
        Container::build(&config)?;
        signals::check()?;

        // Setup the public key infrastructure
        let pki = Pki::new(&config, &network)?;
        signals::check()?;

        // Setup the configs
        let kubeconfig = KubeConfig::new(&config, &pki)?;
        let kubectl = Kubectl::new(kubeconfig.admin());
        let encryptionconfig = EncryptionConfig::new(&config)?;
        signals::check()?;

        let ctx = ClusterContext {
            config: &config,
            network: &network,
            pki: &pki,
            kubeconfig: &kubeconfig,
            encryptionconfig: &encryptionconfig,
            kubectl: &kubectl,
        };

        // Removed again on drop, also if anything below fails
        let nat_exemption = network.setup_nat_exemption(config.root())?;
        let (processes, failures) = Self::register_components(&config).run(&ctx);

        let spawn_shell = !config.no_shell();
        let addon_shutdown = Arc::new(AtomicBool::new(false));
        let mut kubernix = Kubernix {
            addon_shutdown: Arc::clone(&addon_shutdown),
            addon_thread: None,
            config,
            network,
            kubectl,
            processes,
            nat_exemption,
            bridges: Some(bridges),
            system: Some(system),
            _lock: lock,
        };

        // Clear the progress bar before the cleanup on errors
        if !failures.is_empty() {
            drop(p);
            bail!("Unable to start all processes: {}", failures.join("; "))
        }
        if let Err(e) = kubernix.write_env_file() {
            drop(p);
            return Err(e);
        }

        kubernix.spawn_addons(addon_shutdown);
        if let Err(e) = kubernix
            .wait_default_service_account()
            .and_then(|()| kubernix.write_pid_file())
            .and_then(|()| signals::check())
        {
            drop(p);
            return Err(e);
        }
        info!("Everything is up and running");
        p.reset();
        Stage::Running.set();

        if spawn_shell {
            kubernix.spawn_shell()
        } else {
            kubernix.wait()
        }
    }

    fn register_components(config: &Config) -> ComponentRegistry {
        let mut registry = ComponentRegistry::new();
        registry.register(Box::new(etcd::EtcdComponent));
        registry.register(Box::new(apiserver::ApiServerComponent));
        registry.register(Box::new(controllermanager::ControllerManagerComponent));
        registry.register(Box::new(scheduler::SchedulerComponent));
        for node in 0..config.nodes() {
            match config.cri_runtime() {
                config::CriRuntime::Crio => {
                    registry.register(Box::new(crio::CrioComponent::new(node)));
                }
                config::CriRuntime::Containerd => {
                    registry.register(Box::new(containerd::ContainerdComponent::new(node)));
                }
            }
            registry.register(Box::new(kubelet::KubeletComponent::new(node)));
        }
        registry.register(Box::new(proxy::ProxyComponent));
        registry
    }

    /// Wait for the default service account, which pods need, and fail
    /// right away if a component died meanwhile.
    fn wait_default_service_account(&mut self) -> Result<()> {
        let processes = &mut self.processes;
        self.kubectl
            .wait_default_service_account(|| Self::any_died(processes))
    }

    /// Whether a started process died unexpectedly.
    fn any_died(processes: &mut [Stoppables]) -> bool {
        processes.iter_mut().flatten().any(|p| p.died())
    }

    fn spawn_addons(&mut self, addon_shutdown: Arc<AtomicBool>) {
        let addon_config = self.config.clone();
        let addon_network = self.network.clone();
        let addon_kubeconfig = self.kubectl.kubeconfig().to_path_buf();
        // Log before spawning, the interactive shell may own the terminal
        // while the addons get deployed
        if addon_config.addons().contains(&config::Addon::Coredns) {
            info!("Deploying CoreDNS in the background");
        }
        self.addon_thread = Some(thread::spawn(move || {
            let kubectl = Kubectl::new(&addon_kubeconfig);
            if addon_shutdown.load(Ordering::Relaxed) {
                return;
            }
            if addon_config.addons().contains(&config::Addon::Coredns)
                && let Err(e) =
                    CoreDns::apply(&addon_config, &addon_network, &kubectl, &addon_shutdown)
                && !addon_shutdown.load(Ordering::Relaxed)
            {
                error!("Failed to deploy CoreDNS addon: {:#}", e);
            }
        }));
    }

    /// Wait until a termination signal occurs or a component dies
    fn wait(&mut self) -> Result<()> {
        info!("Cluster running, stop it with Ctrl+C or 'kubernix stop'");
        loop {
            if signals::requested() {
                return Ok(());
            }
            if Self::any_died(&mut self.processes) {
                bail!("A cluster component died unexpectedly, shutting down")
            }
            sleep(Duration::from_millis(100));
        }
    }

    /// Spawn a new interactive default system shell
    fn spawn_shell(&mut self) -> Result<()> {
        info!("Spawning interactive shell");
        info!("Please be aware that the cluster stops if you exit the shell");

        let mut cmd = Command::new(self.config.shell_ok()?);
        cmd.current_dir(self.config.root());
        Self::apply_env_file(&Self::env_file(&self.config), &mut cmd)?;
        Self::remove_internal_env(&mut cmd);
        cmd.envs(Self::shell_env(&self.config));

        // An interrupt from the terminal is meant for the shell, but the
        // other termination signals stop the cluster.
        signals::set_shell(true);
        let result = cmd
            .spawn()
            .context("Unable to spawn shell")
            .and_then(|mut child| self.wait_shell(&mut child));
        signals::set_shell(false);
        result
    }

    /// Wait until the shell exits or a termination signal occurs. A died
    /// component only gets reported, the shell stays usable.
    fn wait_shell(&mut self, child: &mut Child) -> Result<()> {
        let mut died = false;
        loop {
            if child.try_wait()?.is_some() {
                return Ok(());
            }
            if signals::requested() {
                info!("Received termination signal, stopping the shell");
                Self::kill_shell(child);
                return Ok(());
            }
            if !died && Self::any_died(&mut self.processes) {
                died = true;
                warn!(
                    "A cluster component died unexpectedly, the cluster may not work \
                     until you exit the shell and start it again"
                );
            }
            sleep(Duration::from_millis(100));
        }
    }

    /// Remove the internal environment variables, which pass the state
    /// between the stages of kubernix, from a command which is not a stage.
    /// Otherwise kubernix started from it would skip its own stages.
    pub(crate) fn remove_internal_env(cmd: &mut Command) {
        cmd.env_remove(ROOTLESS_ENV);
        cmd.env_remove(CLUSTER_SHELL_ENV);
        Nix::remove_internal_env(cmd);
        RunLock::forget(cmd);
    }

    /// Stop the interactive shell, which may ignore SIGTERM but not SIGHUP.
    fn kill_shell(child: &mut Child) {
        if let Ok(pid) = i32::try_from(child.id()) {
            let _ = kill(Pid::from_raw(pid), Signal::SIGHUP);
        }
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if !matches!(child.try_wait(), Ok(None)) {
                return;
            }
            sleep(Duration::from_millis(100));
        }
        let _ = child.kill();
        let _ = child.wait();
    }

    /// Parse the env file and apply its variables to a Command.
    /// Handles `export KEY=VALUE` and `KEY=VALUE` formats, removing the
    /// shell quoting of the values.
    fn apply_env_file(env_file: &Path, cmd: &mut Command) -> Result<()> {
        let content = fs::read_to_string(env_file)
            .with_context(|| format!("Unable to read env file '{}'", env_file.display()))?;
        for line in content.lines() {
            let line = line.strip_prefix("export ").unwrap_or(line);
            if let Some((key, value)) = line.split_once('=') {
                cmd.env(key, shell::unquote(value));
            }
        }
        Ok(())
    }

    /// Lay out the env file
    fn write_env_file(&self) -> Result<()> {
        info!("Writing environment file");
        write_file(
            &Self::env_file(&self.config),
            Self::env_file_content(&[
                (
                    RUNTIME_ENV,
                    cri::cri_socket(&self.config, &self.network, 0)?.to_socket_string(),
                ),
                (
                    "KUBECONFIG",
                    self.kubectl.kubeconfig().display().to_string(),
                ),
            ])
            .as_bytes(),
        )
        .context("Unable to write environment file")
    }

    /// The env file content for the provided variables, with shell quoted
    /// values.
    fn env_file_content(vars: &[(&str, String)]) -> String {
        vars.iter()
            .map(|(key, value)| format!("export {}={}\n", key, shell::quote(value)))
            .collect()
    }

    /// Retrieve the path to the env file
    fn env_file(config: &Config) -> PathBuf {
        config.root().join("kubernix.env")
    }

    /// Retrieve the path to the PID file
    fn pid_file(config: &Config) -> PathBuf {
        config.root().join(PID_FILE)
    }

    /// Write the PID file, which indicates that the cluster is ready
    fn write_pid_file(&self) -> Result<()> {
        let pid_file = Self::pid_file(&self.config);
        debug!("Writing pid file to: {}", pid_file.display());
        write_file(&pid_file, id().to_string().as_bytes()).context("Unable to write pid file")
    }

    /// Remove the PID file if it exists
    fn remove_pid_file(config: &Config) {
        let pid_file = Self::pid_file(config);
        if let Err(e) = fs::remove_file(&pid_file)
            && e.kind() != ErrorKind::NotFound
        {
            warn!("Unable to remove '{}': {}", pid_file.display(), e);
        }
    }

    /// Remove all stale mounts
    fn umount(&self) {
        if self.config.is_rootless() {
            debug!("Skipping mount cleanup in rootless mode");
            return;
        }
        debug!("Removing active mounts");
        Self::umount_below(self.config.root(), Duration::from_secs(15));
    }

    /// Remove CRI storage directories while still inside the user namespace.
    /// Image layers contain files owned by unmapped uids that become
    /// unremovable after rootlesskit exits. Unmounts any lingering
    /// container overlays first so remove_dir_all can succeed. Runs on
    /// shutdown and on startup, for the leftovers of an unclean exit.
    fn cleanup_rootless_storage(config: &Config) {
        if !config.is_rootless() {
            return;
        }
        debug!("Removing rootless CRI storage");
        let root = config.root();
        for entry in ["crio", "containerd"] {
            let dir = root.join(entry);
            if !dir.exists() {
                continue;
            }
            Self::umount_below(&dir, Duration::ZERO);
            Self::remove_rootless_storage(&dir);
        }
    }

    /// Remove the storage and state directories of all nodes below the CRI
    /// directory, but keep the logs and configs of the nodes. Only the
    /// storage contains files of unmapped users, which cannot be removed
    /// from outside of the user namespace.
    fn remove_rootless_storage(dir: &Path) {
        let Ok(nodes) = fs::read_dir(dir) else {
            return;
        };
        for node in nodes.flatten().map(|x| x.path()) {
            for storage in Self::ROOTLESS_STORAGE_DIRS
                .iter()
                .copied()
                .chain(config::OciRuntime::ALL.iter().map(|x| x.name()))
            {
                let path = node.join(storage);
                if path.exists()
                    && let Err(e) = fs::remove_dir_all(&path)
                {
                    debug!("Unable to remove '{}': {}", path.display(), e);
                }
            }
        }
    }

    /// Unmount everything below the root, deepest first. Busy mounts get
    /// retried until the timeout passed and are lazily detached afterwards.
    fn umount_below(root: &Path, timeout: Duration) {
        let deadline = Instant::now() + timeout;
        loop {
            let mount_points = match Self::read_mount_points(root) {
                Ok(mount_points) => mount_points,
                Err(e) => {
                    warn!("Unable to retrieve mounts: {:#}", e);
                    return;
                }
            };
            if mount_points.is_empty() {
                return;
            }
            if Instant::now() >= deadline {
                break;
            }
            for dest in &mount_points {
                debug!("Removing mount: {}", dest.display());
                if let Err(e) = umount2(dest, MntFlags::empty()) {
                    debug!("Unable to umount '{}': {}", dest.display(), e);
                }
            }
            sleep(Duration::from_millis(500));
        }

        for dest in Self::read_mount_points(root).unwrap_or_default() {
            debug!("Detaching mount: {}", dest.display());
            if let Err(e) = umount2(&dest, MntFlags::MNT_DETACH) {
                debug!("Unable to detach '{}': {}", dest.display(), e);
            }
        }
        for dest in Self::read_mount_points(root).unwrap_or_default() {
            warn!(
                "Unable to remove mount '{}', may need manual cleanup",
                dest.display()
            );
        }
    }

    /// The CRI storage and state directories of a node, removed on rootless
    /// shutdown in addition to the OCI runtime state directories.
    const ROOTLESS_STORAGE_DIRS: &[&str] = &["containers", "root", "state"];

    /// Read mount points from /proc/mounts filtered by the given root path,
    /// sorted deepest-first for safe unmounting.
    fn read_mount_points(root: &Path) -> Result<Vec<PathBuf>> {
        let file = fs::File::open("/proc/mounts").context("Unable to open /proc/mounts")?;
        let reader = BufReader::new(file);
        let mut points: Vec<PathBuf> = reader
            .lines()
            .map_while(Result::ok)
            .filter_map(|line| {
                line.split_whitespace()
                    .nth(1)
                    .map(Self::unescape_mount_path)
            })
            .filter(|p| p.starts_with(root) && p != root)
            .collect();
        points.sort_by_key(|p| std::cmp::Reverse(p.components().count()));
        Ok(points)
    }

    /// Decode the octal escapes of /proc/mounts paths, for example `\040`
    /// for a space.
    fn unescape_mount_path(path: &str) -> PathBuf {
        let bytes = path.as_bytes();
        let mut out = Vec::with_capacity(bytes.len());
        let mut i = 0;
        while i < bytes.len() {
            let escape = bytes.get(i + 1..i + 4).filter(|digits| {
                bytes[i] == b'\\' && digits.iter().all(|d| (b'0'..=b'7').contains(d))
            });
            match escape {
                Some(digits) => {
                    let value = digits
                        .iter()
                        .fold(0u32, |acc, d| acc * 8 + u32::from(d - b'0'));
                    out.push(u8::try_from(value).unwrap_or(b'?'));
                    i += 4;
                }
                None => {
                    out.push(bytes[i]);
                    i += 1;
                }
            }
        }
        PathBuf::from(OsString::from_vec(out))
    }
}

/// The lifecycle stage of the cluster, which decides how the force quit
/// watcher reports the first termination signal.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
enum Stage {
    Bootstrap,
    Running,
    Cleanup,
    Done,
}

static STAGE: AtomicU8 = AtomicU8::new(Stage::Bootstrap as u8);

/// Set once the first termination signal got reported, or the cleanup
/// started because of it.
static SIGNAL_NOTICED: AtomicBool = AtomicBool::new(false);

/// The restore of the hosts file, which a force quit still does.
type HostsRestore = Box<dyn FnOnce() + Send>;
static FORCE_QUIT_HOSTS: Mutex<Option<HostsRestore>> = Mutex::new(None);

impl Stage {
    fn get() -> Self {
        match STAGE.load(Ordering::SeqCst) {
            0 => Stage::Bootstrap,
            1 => Stage::Running,
            2 => Stage::Cleanup,
            _ => Stage::Done,
        }
    }

    fn set(self) {
        STAGE.store(self as u8, Ordering::SeqCst);
    }
}

impl Kubernix {
    /// Force quit on another termination signal during the bootstrap or
    /// the cleanup. If the cleanup did not start because of a signal, for
    /// example after the shell exited, it takes two signals.
    fn watch_force_quit(config: &Config) {
        let pid_file = Self::pid_file(config);
        let rootless_root = config.is_rootless().then(|| config.root().to_path_buf());
        thread::spawn(move || {
            loop {
                sleep(Duration::from_millis(50));
                let stage = Stage::get();
                if stage == Stage::Done {
                    return;
                }
                if signals::forced() {
                    Self::force_quit(&pid_file, rootless_root.as_deref());
                }
                if signals::requested() && !SIGNAL_NOTICED.swap(true, Ordering::SeqCst) {
                    match stage {
                        Stage::Bootstrap => warn!(
                            "Received termination signal, aborting the bootstrap, \
                             send another one to force quit"
                        ),
                        Stage::Cleanup => warn!(
                            "Received termination signal during the cleanup, \
                             send another one to force quit"
                        ),
                        Stage::Running | Stage::Done => {}
                    }
                }
            }
        });
    }

    /// Kill all process groups, restore the hosts file, remove the PID
    /// file and exit, which releases the lock.
    fn force_quit(pid_file: &Path, rootless_root: Option<&Path>) -> ! {
        warn!(
            "Received another termination signal, force quitting, pods, node containers, \
             mounts, network interfaces and firewall rules may need a manual cleanup"
        );
        process::kill_all();
        if let Some(restore) = FORCE_QUIT_HOSTS.lock().ok().and_then(|mut r| r.take()) {
            restore();
        }
        if let Err(e) = fs::remove_file(pid_file)
            && e.kind() != ErrorKind::NotFound
        {
            warn!("Unable to remove '{}': {}", pid_file.display(), e);
        }
        if let Some(root) = rootless_root {
            warn!("{}", Self::rootless_storage_hint(root));
        }
        if let Some(p) = Progress::get() {
            p.abandon();
        }
        std::process::exit(1);
    }

    /// The hint for the CRI storage which a rootless force quit leaves
    /// behind, since only a user namespace can remove it.
    fn rootless_storage_hint(root: &Path) -> String {
        format!(
            "The rootless CRI storage contains files of subordinate users, the next \
             start in the same run directory removes it, or remove the run directory \
             with: {}",
            Config::remove_command(root, true)
        )
    }
}

impl Drop for Kubernix {
    fn drop(&mut self) {
        let p = Progress::new(
            Self::processes(&self.config),
            self.config.log_level(),
            self.config.log_format(),
        );

        info!("Cleaning up");
        if signals::requested() {
            SIGNAL_NOTICED.store(true, Ordering::SeqCst);
        }
        Stage::Cleanup.set();

        self.addon_shutdown.store(true, Ordering::Relaxed);
        if let Some(handle) = self.addon_thread.take() {
            debug!("Waiting for addon deployment to finish");
            let deadline = Instant::now() + Duration::from_secs(5);
            while !handle.is_finished() && Instant::now() < deadline {
                sleep(Duration::from_millis(10));
            }
            if handle.is_finished() {
                let _ = handle.join();
            } else {
                debug!("Addon thread did not finish in time, detaching");
            }
        }

        self.stop();
        self.umount();
        Self::cleanup_rootless_storage(&self.config);
        // Remove the firewall rule and the bridges, and restore the host
        // state, while another signal still force quits
        drop(self.nat_exemption.take());
        drop(self.bridges.take());
        drop(self.system.take());
        Self::remove_pid_file(&self.config);
        Stage::Done.set();
        info!("Cleanup done");

        p.reset();
        debug!("All done");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::component::Phase;
    use std::io::Write;
    use tempfile::tempdir;

    #[test]
    fn register_components_orders_phases() {
        let names = |args: &[&str]| -> Vec<(Phase, Vec<String>)> {
            let config = config::tests::parse(args).unwrap();
            Kubernix::register_components(&config)
                .by_phase()
                .into_iter()
                .map(|(phase, c)| (phase, c.iter().map(|c| c.name().to_owned()).collect()))
                .collect()
        };
        let expected = |runtime: &str| {
            vec![
                (Phase::Infrastructure, vec!["etcd".to_owned()]),
                (Phase::ControlPlane, vec!["API Server".to_owned()]),
                (
                    Phase::Controller,
                    vec![
                        "Controller Manager".to_owned(),
                        "Scheduler".to_owned(),
                        format!("{} (node 0)", runtime),
                        format!("{} (node 1)", runtime),
                        "Proxy".to_owned(),
                    ],
                ),
                (
                    Phase::NodeAgent,
                    vec!["Kubelet (node 0)".to_owned(), "Kubelet (node 1)".to_owned()],
                ),
            ]
        };
        assert_eq!(
            names(&["--nodes=2", "--cri-runtime=crio"]),
            expected("CRI-O")
        );
        assert_eq!(
            names(&["--nodes=2", "--cri-runtime=containerd"]),
            expected("Containerd")
        );
    }

    #[test]
    fn content_hash_depends_on_inputs() {
        let a = content_hash(&[b"crun", b"df"]);
        assert_eq!(a, content_hash(&[b"crun", b"df"]));
        assert_ne!(a, content_hash(&[b"crun runc", b"df"]));
        assert_ne!(a, content_hash(&[b"crund", b"f"]));
    }

    #[test]
    fn remove_rootless_storage_keeps_logs_and_configs() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let node = dir.path().join("node-0");
        for storage in [
            "containers/storage",
            "root",
            "state",
            "crun",
            "runc",
            "crio.conf.d",
        ] {
            fs::create_dir_all(node.join(storage))?;
        }
        fs::write(node.join("crio.log"), "log")?;
        Kubernix::remove_rootless_storage(dir.path());
        let mut left = fs::read_dir(&node)?
            .map(|x| Ok(x?.file_name().to_string_lossy().into_owned()))
            .collect::<Result<Vec<_>>>()?;
        left.sort();
        assert_eq!(left, ["crio.conf.d", "crio.log"]);
        Ok(())
    }

    #[test]
    fn apply_env_file_parses_exports() {
        let dir = tempdir().unwrap();
        let env_file = dir.path().join("test.env");
        let mut f = fs::File::create(&env_file).unwrap();
        writeln!(f, "export FOO=bar").unwrap();
        writeln!(f, "BAZ=qux").unwrap();
        writeln!(f, "QUOTED=\"hello world\"").unwrap();
        writeln!(f, "SINGLE='single'").unwrap();

        writeln!(f, "ESCAPED='it'\\''s a /path with spaces'").unwrap();
        writeln!(f).unwrap();

        let mut cmd = Command::new("echo");
        Kubernix::apply_env_file(&env_file, &mut cmd).unwrap();
        assert_eq!(
            envs(&cmd),
            vec![
                ("BAZ".into(), "qux".into()),
                ("ESCAPED".into(), "it's a /path with spaces".into()),
                ("FOO".into(), "bar".into()),
                ("QUOTED".into(), "hello world".into()),
                ("SINGLE".into(), "single".into()),
            ]
        );
    }

    fn envs(cmd: &Command) -> Vec<(String, String)> {
        let mut envs: Vec<(String, String)> = cmd
            .get_envs()
            .map(|(k, v)| {
                (
                    k.to_string_lossy().into(),
                    v.unwrap_or_default().to_string_lossy().into(),
                )
            })
            .collect();
        envs.sort();
        envs
    }

    #[test]
    fn env_file_roundtrip() {
        let dir = tempdir().unwrap();
        let env_file = dir.path().join("test.env");
        let vars = [
            ("KUBECONFIG", "/run dir/it's/admin.kubeconfig".to_string()),
            ("PLAIN", "unix:///run/crio.sock".to_string()),
        ];
        fs::write(&env_file, Kubernix::env_file_content(&vars)).unwrap();

        let mut cmd = Command::new("echo");
        Kubernix::apply_env_file(&env_file, &mut cmd).unwrap();
        let expected: Vec<(String, String)> = vars
            .iter()
            .map(|(k, v)| (k.to_string(), v.clone()))
            .collect();
        assert_eq!(envs(&cmd), expected);

        // The file is valid shell
        let output = Command::new("sh")
            .arg("-c")
            .arg(format!(
                ". '{}' && printf %s \"$KUBECONFIG\"",
                env_file.display()
            ))
            .output()
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&output.stdout),
            "/run dir/it's/admin.kubeconfig"
        );
    }

    #[test]
    fn cgroup_evacuate_snippet() {
        let snippet = Kubernix::cgroup_evacuate("$CGRP", "init");
        assert!(snippet.contains("mkdir -p \"$CGRP/init\"; "));
        assert!(snippet.contains("echo $p >\"$CGRP/init/cgroup.procs\""));
        assert!(snippet.contains("for c in memory cpu pids io; do "));
        assert!(snippet.contains(">\"$CGRP/cgroup.subtree_control\""));
        assert!(snippet.ends_with("; "));
        // Warnings go through the logger instead
        assert!(!snippet.contains("echo '"));
    }

    #[test]
    fn rootless_storage_hint_quotes_root() {
        let hint = Kubernix::rootless_storage_hint(Path::new("/run dir"));
        assert!(
            hint.ends_with("unshare --map-auto --map-root-user rm -rf '/run dir'"),
            "{}",
            hint
        );
    }

    #[test]
    fn unescape_mount_path_octal() {
        assert_eq!(
            Kubernix::unescape_mount_path(r"/run\040dir/a\011b\134c"),
            PathBuf::from("/run dir/a\tb\\c")
        );
        assert_eq!(
            Kubernix::unescape_mount_path(r"/plain\04"),
            PathBuf::from(r"/plain\04")
        );
        assert_eq!(
            Kubernix::unescape_mount_path(r"/no\999escape"),
            PathBuf::from(r"/no\999escape")
        );
    }

    #[test]
    fn lock_is_exclusive() -> Result<()> {
        let config = config::tests::test_config()?;
        let lock = Kubernix::lock(&config)?;
        let err = Kubernix::lock(&config)
            .err()
            .context("expected error")?
            .to_string();
        assert!(err.contains("already running"), "{}", err);
        drop(lock);
        assert!(runlock::tests::released(&Kubernix::lock_file(&config))?);
        Kubernix::lock(&config)?;
        Ok(())
    }

    #[test]
    fn running_pid_requires_running_instance() -> Result<()> {
        let config = config::tests::test_config()?;
        let err = Kubernix::running_pid(&config).unwrap_err().to_string();
        assert!(err.contains("No running kubernix cluster"), "{}", err);

        let _lock = Kubernix::lock(&config)?;
        let err = Kubernix::running_pid(&config).unwrap_err().to_string();
        assert!(err.contains("not ready yet"), "{}", err);

        fs::write(Kubernix::pid_file(&config), id().to_string())?;
        assert_eq!(Kubernix::running_pid(&config)?, i32::try_from(id())?);

        Kubernix::remove_pid_file(&config);
        assert!(!Kubernix::pid_file(&config).exists());
        Ok(())
    }

    #[test]
    fn state_follows_lock_and_pid_file() -> Result<()> {
        let config = config::tests::test_config()?;
        assert_eq!(Kubernix::state(&config)?, State::Stopped);

        // A stale PID file without the lock does not count
        fs::write(Kubernix::pid_file(&config), id().to_string())?;
        assert_eq!(Kubernix::state(&config)?, State::Stopped);
        assert_eq!(RunLock::holder(&Kubernix::lock_file(&config))?, None);

        // Taking the lock removes the stale PID file
        let lock = Kubernix::lock(&config)?;
        let pid = i32::try_from(id())?;
        assert_eq!(
            RunLock::holder(&Kubernix::lock_file(&config))?,
            Some(Some(pid))
        );
        assert!(!Kubernix::pid_file(&config).exists());
        assert_eq!(Kubernix::state(&config)?, State::Starting(Some(pid)));

        fs::write(Kubernix::pid_file(&config), id().to_string())?;
        assert_eq!(
            Kubernix::state(&config)?,
            State::Running(i32::try_from(id())?)
        );

        fs::write(Kubernix::pid_file(&config), "invalid")?;
        assert!(Kubernix::state(&config).is_err());

        // An empty PID file is not written completely yet
        fs::write(Kubernix::pid_file(&config), "\n")?;
        assert_eq!(Kubernix::state(&config)?, State::Starting(Some(pid)));

        drop(lock);
        assert!(runlock::tests::released(&Kubernix::lock_file(&config))?);
        assert_eq!(Kubernix::state(&config)?, State::Stopped);
        Ok(())
    }

    #[test]
    fn remove_internal_env_removes_markers() {
        let mut cmd = Command::new("true");
        Kubernix::remove_internal_env(&mut cmd);
        let removed: Vec<String> = cmd
            .get_envs()
            .filter(|(_, v)| v.is_none())
            .map(|(k, _)| k.to_string_lossy().into())
            .collect();
        assert_eq!(
            removed,
            [
                "_KUBERNIX_CLUSTER_SHELL",
                "_KUBERNIX_IN_NIX",
                "_KUBERNIX_LOCK_FD",
                "_KUBERNIX_ROOTLESS"
            ]
        );
    }

    #[test]
    fn shell_args_interactive() -> Result<()> {
        let config = config::tests::test_config()?;
        let env_file = Path::new("/run dir/kubernix.env");
        let args = Kubernix::shell_args(&config, 42, &[], env_file, Some(Path::new("/cwd")), &[])?;
        assert_eq!(args[..2], ["bash", "-c"]);
        assert_eq!(
            args[2],
            format!(
                ". '/run dir/kubernix.env' && export KUBERNIX_ROOT={} && exec {}",
                shell::quote(&config.root().display().to_string()),
                shell::quote(&config.shell_ok()?)
            )
        );
        assert_eq!(args.len(), 3);
        Ok(())
    }

    #[test]
    fn shell_args_rootless_enter_namespaces() -> Result<()> {
        let config = config::tests::test_config_rootless()?;
        let command = ["kubectl", "get", "nodes"].map(String::from);
        let args = Kubernix::shell_args(
            &config,
            42,
            &["-U", "-m", "-n"],
            Path::new("/env"),
            Some(Path::new("/my dir")),
            &command,
        )?;
        assert_eq!(
            args,
            [
                "nsenter",
                "-t",
                "42",
                "-U",
                "-m",
                "-n",
                "--preserve-credentials",
                "--wd=/my dir",
                "bash",
                "-c",
                &format!(
                    ". '/env' && export KUBERNIX_ROOT={root} && \
                     export _KUBERNIX_CLUSTER_SHELL={root} && exec \"$@\"",
                    root = shell::quote(&config.root().display().to_string())
                ),
                "kubernix",
                "kubectl",
                "get",
                "nodes",
            ]
        );

        // Without the user namespace, the credentials stay unchanged
        let args = Kubernix::shell_args(&config, 42, &["-n"], Path::new("/env"), None, &command)?;
        assert_eq!(args[..5], ["nsenter", "-t", "42", "-n", "bash"]);
        Ok(())
    }

    #[test]
    fn write_config_only_new_files() -> Result<()> {
        let config = config::tests::test_config()?;
        assert!(Kubernix::write_config(&config, ConfigFile::Missing)?);
        assert!(config.file_exists());

        // The file of another instance which took the lock first is kept
        fs::write(config.file(), "other")?;
        let err = Kubernix::write_config(&config, ConfigFile::Missing).unwrap_err();
        assert!(err.to_string().contains("in the meantime"), "{err}");
        assert!(!Kubernix::write_config(&config, ConfigFile::Current)?);
        assert_eq!(fs::read_to_string(config.file())?, "other");

        let migrated = Box::new((*config).clone());
        assert!(!Kubernix::write_config(
            &config,
            ConfigFile::Migrated(migrated)
        )?);
        assert_ne!(fs::read_to_string(config.file())?, "other");
        Ok(())
    }

    #[test]
    fn shell_namespaces_skips_shared() -> Result<()> {
        let pid = i32::try_from(std::process::id())?;
        temp_env::with_var_unset(ROOTLESS_ENV, || -> Result<()> {
            // The own namespaces are shared
            let config = config::tests::test_config_rootless()?;
            assert!(Kubernix::shell_namespaces(&config, pid).is_empty());

            // Unknown ones get entered
            assert_eq!(
                Kubernix::shell_namespaces(&config, i32::MAX),
                ["-U", "-m", "-n"]
            );

            // Root mode does not enter any
            let config = config::tests::test_config()?;
            assert!(Kubernix::shell_namespaces(&config, i32::MAX).is_empty());
            Ok(())
        })
    }

    #[test]
    fn shell_args_command_keeps_exit_code_and_env() -> Result<()> {
        let config = config::tests::test_config()?;
        let env_file = config.root().join("kubernix.env");
        fs::write(
            &env_file,
            Kubernix::env_file_content(&[("KUBECONFIG", "/it's/admin".into())]),
        )?;
        let command = [
            "sh",
            "-c",
            "printf '%s %s %s %s' \"$KUBECONFIG\" \"$1\" \"$KUBERNIX_ROOT\" \
             \"${_KUBERNIX_CLUSTER_SHELL-unset}\"; exit 7",
            "sh",
            "a 'b'",
        ]
        .map(String::from);
        let run = |config: &Config| -> Result<(Option<i32>, String)> {
            let args = Kubernix::shell_args(config, 0, &[], &env_file, None, &command)?;
            // A relative root in the environment of the caller gets replaced
            let output = Command::new(&args[0])
                .args(&args[1..])
                .env(ROOT_ENV, "kubernix-run")
                .env_remove(CLUSTER_SHELL_ENV)
                .output()?;
            Ok((
                output.status.code(),
                String::from_utf8_lossy(&output.stdout).into(),
            ))
        };
        let root = config.root().display();
        assert_eq!(
            run(&config)?,
            (Some(7), format!("/it's/admin a 'b' {} unset", root))
        );

        // The shells of a rootless cluster are marked
        let mut rootless = (*config).clone();
        rootless.set_rootless(true);
        assert_eq!(
            run(&rootless)?,
            (Some(7), format!("/it's/admin a 'b' {} {}", root, root))
        );
        Ok(())
    }

    #[test]
    fn shell_env_marks_rootless_shells() -> Result<()> {
        let config = config::tests::test_config()?;
        let root = config.root().display().to_string();
        assert_eq!(Kubernix::shell_env(&config), [(ROOT_ENV, root.clone())]);
        let mut cmd = Command::new("true");
        Kubernix::remove_internal_env(&mut cmd);
        cmd.envs(Kubernix::shell_env(&config));
        assert!(envs(&cmd).contains(&(ROOT_ENV.into(), root.clone())));

        let config = config::tests::test_config_rootless()?;
        let root = config.root().display().to_string();
        assert_eq!(
            Kubernix::shell_env(&config),
            [(ROOT_ENV, root.clone()), (CLUSTER_SHELL_ENV, root)]
        );
        Ok(())
    }

    #[test]
    fn rootless_mode_decision() -> Result<()> {
        let mode = |root, userns, stage, shell, bootstrap| {
            Kubernix::rootless_mode(root, userns, stage, shell, bootstrap)
        };
        for bootstrap in [false, true] {
            // Regular users
            assert!(mode(false, false, false, false, bootstrap)?);
            assert!(mode(false, true, false, true, bootstrap)?);
            // Real root, where the markers do not count
            assert!(!mode(true, false, false, false, bootstrap)?);
            assert!(!mode(true, false, true, true, bootstrap)?);
            // Root in a rootless or user namespaced container
            assert!(!mode(true, true, false, false, bootstrap)?);
            // Inside rootlesskit
            assert!(mode(true, true, true, false, bootstrap)?);
            assert!(mode(true, true, true, true, bootstrap)?);
        }
        // The shell of a rootless cluster runs the subcommands, but cannot
        // start a cluster
        assert!(mode(true, true, false, true, false)?);
        let err = mode(true, true, false, true, true).unwrap_err().to_string();
        assert!(err.contains("shell of a rootless cluster"), "{}", err);
        Ok(())
    }

    #[test]
    fn node_status_parse() {
        assert_eq!(
            NodeStatus::parse("node-0 True\nnode-1 False\nnode-2\n\n"),
            vec![
                NodeStatus {
                    name: "node-0".into(),
                    ready: true
                },
                NodeStatus {
                    name: "node-1".into(),
                    ready: false
                },
                NodeStatus {
                    name: "node-2".into(),
                    ready: false
                },
            ]
        );
        assert!(NodeStatus::TEMPLATE.starts_with("go-template="));
    }

    fn test_status() -> Status {
        Status {
            root: "/kubernix-run".into(),
            state: State::Running(42).name(),
            pid: Some(42),
            cluster: Some(ClusterStatus {
                mode: "rootless",
                nodes: 2,
                cri_runtime: CriRuntime::Containerd,
                oci_runtime: OciRuntime::Runc,
                api_server: "https://127.0.0.1:6443".into(),
                node_status: Some(vec![
                    NodeStatus {
                        name: "node-0".into(),
                        ready: true,
                    },
                    NodeStatus {
                        name: "node-1".into(),
                        ready: false,
                    },
                ]),
            }),
        }
    }

    #[test]
    fn status_text() {
        assert_eq!(
            test_status().text(),
            concat!(
                "Root:        /kubernix-run\n",
                "State:       running (PID 42)\n",
                "Mode:        rootless\n",
                "Nodes:       2\n",
                "CRI runtime: containerd\n",
                "OCI runtime: runc\n",
                "API server:  https://127.0.0.1:6443\n",
                "Node:        node-0 Ready\n",
                "Node:        node-1 NotReady\n",
            )
        );

        let stopped = Status {
            root: "/kubernix-run".into(),
            state: State::Stopped.name(),
            pid: None,
            cluster: None,
        };
        assert_eq!(
            stopped.text(),
            "Root:        /kubernix-run\nState:       stopped\n"
        );
    }

    #[test]
    fn status_json() -> Result<()> {
        let v = serde_json::to_value(test_status())?;
        assert_eq!(
            v,
            serde_json::json!({
                "root": "/kubernix-run",
                "state": "running",
                "pid": 42,
                "mode": "rootless",
                "nodes": 2,
                "cri-runtime": "containerd",
                "oci-runtime": "runc",
                "api-server": "https://127.0.0.1:6443",
                "node-status": [
                    {"name": "node-0", "ready": true},
                    {"name": "node-1", "ready": false},
                ],
            })
        );

        let starting = Status {
            root: "/r".into(),
            state: State::Starting(None).name(),
            pid: None,
            cluster: None,
        };
        assert_eq!(
            serde_json::to_value(starting)?,
            serde_json::json!({"root": "/r", "state": "starting"})
        );
        Ok(())
    }

    #[test]
    fn read_mount_points_empty_for_nonexistent_root() {
        let points = Kubernix::read_mount_points(Path::new("/nonexistent")).unwrap();
        assert!(points.is_empty());
    }

    #[test]
    fn read_mount_points_excludes_root_itself() {
        let dir = tempdir().unwrap();
        let points = Kubernix::read_mount_points(dir.path()).unwrap();
        assert!(points.is_empty());
    }
}
