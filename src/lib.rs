//! Kubernetes development cluster bootstrapping with Nix packages.
//!
//! This crate implements the `kubernix` binary, which bootstraps a local
//! Kubernetes cluster for testing and development. All components (etcd,
//! the Kubernetes binaries, CRI-O or containerd, crun and runc, the CNI
//! plugins, etc.) come from a pinned Nix environment, so Nix with flakes
//! enabled is the only real host dependency.
//!
//! The library mainly exists for the binary: [`Config`] holds the command
//! line options (with the [`SubCommand`], [`LogFormat`], [`CriRuntime`] and
//! [`OciRuntime`] option types), [`Logger`] is the log implementation and
//! [`Kubernix`] runs the cluster based on the configuration.
//!
//! [`Kubernix::start`] works in these steps:
//!
//! 1. Write the configuration into the run root (`kubernix-run` by
//!    default) and lock it, so that only one instance uses it. Then write
//!    a Nix flake into it and re-execute the binary inside `nix develop`,
//!    forwarding all options.
//! 2. If not started as root, re-execute again inside `rootlesskit`, which
//!    provides a user namespace where all components run as fake root
//!    (rootless mode).
//! 3. Prepare the system: load the required kernel modules (root mode
//!    only) and set the network sysctls, which root mode skips inside a
//!    container. With multiple nodes in root mode, build the node
//!    container image. Then generate the certificates, kubeconfigs and the
//!    encryption config, and start the components in boot phases: etcd,
//!    the API server, the controllers together with the CRI runtimes and
//!    kube-proxy, and finally the kubelets. Deploy the addons (CoreDNS)
//!    afterwards.
//! 4. Spawn an interactive shell, or wait for a termination signal with
//!    `--no-shell`, and stop all components when it exits.
//!
//! With more than one node, every node gets its own CRI runtime and kubelet.
//! In root mode, they run inside containers (podman by default), while in
//! rootless mode they run as processes on the same host.
//!
//! [`Kubernix::new_shell`] spawns an additional shell for a running cluster,
//! which enters the namespaces of the cluster via `nsenter` in rootless
//! mode.
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
mod scheduler;
mod signals;
mod system;

pub use config::{Config, CriRuntime, LogFormat, OciRuntime, SubCommand};
pub use logger::Logger;

/// Write `content` to `path` only if the file does not exist or its
/// current contents differ. Avoids unnecessary filesystem writes and
/// inode updates on warm restarts.
pub(crate) fn write_if_changed(path: &std::path::Path, content: &str) -> anyhow::Result<()> {
    if path.exists()
        && let Ok(existing) = std::fs::read_to_string(path)
        && existing == content
    {
        return Ok(());
    }
    std::fs::write(path, content)
        .with_context(|| format!("Unable to write '{}'", path.display()))?;
    Ok(())
}

use crate::nix::Nix;
use component::{ClusterContext, ComponentRegistry};
use container::Container;
use coredns::CoreDns;
use cri::RUNTIME_ENV;
use encryptionconfig::EncryptionConfig;
use kubeconfig::KubeConfig;
use kubectl::Kubectl;
use network::Network;
use pki::Pki;
use process::Stoppables;
use progress::Progress;
use system::System;

const ROOTLESS_ENV: &str = "KUBERNIX_ROOTLESS";

/// The lock file in the root, held by the instance running the cluster.
const LOCK_FILE: &str = "kubernix.lock";

/// The PID file in the root, written once the cluster is up.
const PID_FILE: &str = "kubernix.pid";

/// The port the API server listens on.
pub(crate) const API_SERVER_PORT: u16 = 6443;

use ::nix::{
    errno::Errno,
    fcntl::{Flock, FlockArg},
    mount::{MntFlags, umount2},
    sys::signal::{Signal, kill},
    unistd::{Pid, getuid},
};
use anyhow::{Context, Result, bail};
use log::{debug, error, info, set_boxed_logger, warn};
use std::{
    ffi::OsString,
    fs::{self, File, OpenOptions},
    io::{BufRead, BufReader, ErrorKind},
    os::unix::{ffi::OsStringExt, process::CommandExt},
    path::{Path, PathBuf},
    process::{Child, Command, id},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, sleep},
    time::{Duration, Instant},
};

/// The main entry point for the application
pub struct Kubernix {
    addon_shutdown: Arc<AtomicBool>,
    addon_thread: Option<thread::JoinHandle<()>>,
    config: Config,
    network: Network,
    kubectl: Kubectl,
    processes: Vec<Stoppables>,
    // Dropped after the cleanup: restores the system state and releases
    // the run directory.
    _system: System,
    _lock: Flock<File>,
}

impl Kubernix {
    /// Start kubernix by consuming the provided configuration.
    ///
    /// Re-executes the binary inside the Nix environment (and inside
    /// rootlesskit in rootless mode) if necessary, bootstraps the cluster
    /// and blocks until the shell exits or a termination signal arrives.
    pub fn start(mut config: Config) -> Result<()> {
        Self::prepare_env(&mut config, true)?;

        // Fail early if another instance runs on the same root. Exec'ing
        // into the next stage closes the lock file, so that stage takes the
        // lock again.
        let lock = Self::lock(&config)?;

        // Bootstrap if we're not inside a nix shell
        if Nix::is_active() {
            if config.is_rootless() && std::env::var(ROOTLESS_ENV).as_deref() != Ok("1") {
                Network::warn_overlapping_route(config.cidr(), "cluster CIDR")?;
                Network::warn_overlapping_route(config::SLIRP4NETNS_CIDR, "slirp4netns network")?;
                return Self::reexec_rootless();
            }
            Self::bootstrap_cluster(config, lock)
        } else {
            Nix::bootstrap(config)
        }
    }

    /// Spawn a new shell into the provided configuration environment.
    pub fn new_shell(mut config: Config) -> Result<()> {
        Self::prepare_env(&mut config, false)?;
        let pid = Self::running_pid(&config)?;

        info!(
            "Spawning new kubernix shell in: '{}'",
            config.root().display()
        );

        let env_file = Self::env_file(&config);
        if !env_file.exists() {
            bail!(
                "Necessary environment file '{}' does not exist",
                env_file.display()
            )
        }

        let shell_cmd = format!(
            ". {} && exec {}",
            Self::shell_escape(&env_file.display().to_string()),
            Self::shell_escape(&config.shell_ok()?),
        );

        if config.is_rootless() && std::env::var(ROOTLESS_ENV).as_deref() != Ok("1") {
            // Enter the namespaces of the rootlesskit session running the
            // cluster.
            let pid = pid.to_string();
            Nix::exec(
                &config,
                &[
                    "nsenter",
                    "-t",
                    &pid,
                    "-U",
                    "-m",
                    "-n",
                    "--preserve-credentials",
                    "bash",
                    "-c",
                    &shell_cmd,
                ],
            )
        } else {
            Nix::exec(&config, &["bash", "-c", &shell_cmd])
        }
    }

    /// Prepare the environment based on the provided config. A new run
    /// directory gets only created if `bootstrap` is set.
    fn prepare_env(config: &mut Config, bootstrap: bool) -> Result<()> {
        let env_rootless = std::env::var(ROOTLESS_ENV).as_deref() == Ok("1");
        let rootless = if !getuid().is_root() {
            true
        } else if env_rootless {
            // KUBERNIX_ROOTLESS=1 is set by reexec_rootless() inside rootlesskit.
            // Only honor it when actually inside a user namespace to prevent
            // a broken cluster from `sudo KUBERNIX_ROOTLESS=1 kubernix`.
            System::in_user_namespace()
        } else {
            false
        };

        // Set rootless before config loading so that it can be compared
        // with the mode of the existing run directory.
        config.set_rootless(rootless);

        // Prepare the configuration
        if config.root().exists() && (bootstrap || config.file_exists()) {
            config.canonicalize_root()?;
            config.try_load_file()?;
            config.validate()?;
        } else if bootstrap {
            config.validate()?;
            config.canonicalize_root()?;
            config.to_file()?;
        } else {
            bail!("No kubernix cluster found in '{}'", config.root().display())
        }

        // Setup the logger
        set_boxed_logger(Logger::new(config.log_level(), config.log_format()))
            .context("Unable to set logger")?;

        if config.is_rootless() && !Nix::is_active() {
            info!("Running in rootless mode");
        }

        Ok(())
    }

    /// Take the exclusive lock of the run directory, which fails if
    /// another instance uses it.
    fn lock(config: &Config) -> Result<Flock<File>> {
        let path = config.root().join(LOCK_FILE);
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&path)
            .with_context(|| format!("Unable to open lock file '{}'", path.display()))?;
        match Flock::lock(file, FlockArg::LockExclusiveNonblock) {
            Ok(lock) => Ok(lock),
            Err((_, Errno::EWOULDBLOCK)) => {
                let pid = fs::read_to_string(Self::pid_file(config))
                    .map(|pid| format!(" (PID {})", pid.trim()))
                    .unwrap_or_default();
                bail!(
                    "Another kubernix instance{} is already running in '{}', \
                     use 'kubernix shell' to access it or a different root",
                    pid,
                    config.root().display(),
                )
            }
            Err((_, e)) => Err(e).with_context(|| format!("Unable to lock '{}'", path.display())),
        }
    }

    /// Retrieve the PID of the instance running the cluster, which fails
    /// if no cluster is running or it is not ready yet.
    fn running_pid(config: &Config) -> Result<i32> {
        let root = config.root().display();
        let running = File::open(config.root().join(LOCK_FILE)).is_ok_and(|file| {
            matches!(
                Flock::lock(file, FlockArg::LockSharedNonblock),
                Err((_, Errno::EWOULDBLOCK))
            )
        });
        if !running {
            bail!("No running kubernix cluster found in '{}'", root)
        }

        let pid_file = Self::pid_file(config);
        let pid: i32 = match fs::read_to_string(&pid_file) {
            Ok(pid) => pid
                .trim()
                .parse()
                .with_context(|| format!("Invalid PID file '{}'", pid_file.display()))?,
            Err(e) if e.kind() == ErrorKind::NotFound => {
                bail!("The kubernix cluster in '{}' is not ready yet", root)
            }
            Err(e) => {
                return Err(e).with_context(|| format!("Unable to read '{}'", pid_file.display()));
            }
        };
        match kill(Pid::from_raw(pid), None) {
            Ok(()) | Err(Errno::EPERM) => Ok(pid),
            Err(_) => bail!(
                "The kubernix instance (PID {}) in '{}' is not running",
                pid,
                root
            ),
        }
    }

    /// POSIX single-quote escaping: wraps in single quotes and escapes
    /// any embedded single quotes with the '\'' sequence.
    fn shell_escape(s: &str) -> String {
        format!("'{}'", s.replace('\'', "'\\''"))
    }

    /// Remove the shell quoting of a single word, the inverse of
    /// `shell_escape`. Also supports double quotes and backslash escapes.
    fn shell_unquote(s: &str) -> String {
        let mut out = String::with_capacity(s.len());
        let mut chars = s.chars();
        while let Some(c) = chars.next() {
            match c {
                '\'' => out.extend(chars.by_ref().take_while(|&c| c != '\'')),
                '"' => {
                    while let Some(c) = chars.next() {
                        match c {
                            '"' => break,
                            '\\' => out.extend(chars.next()),
                            c => out.push(c),
                        }
                    }
                }
                '\\' => out.extend(chars.next()),
                c => out.push(c),
            }
        }
        out
    }

    /// Shell snippet which moves all processes of the cgroup into a child
    /// cgroup and enables the controllers for the children, since cgroup
    /// v2's "no internal processes" rule forbids it otherwise.
    fn cgroup_evacuate(cgroup: &str, child: &str) -> String {
        format!(
            concat!(
                "mkdir -p \"{cg}/{child}\"; ",
                "for p in $(cat \"{cg}/cgroup.procs\"); do ",
                "echo $p >\"{cg}/{child}/cgroup.procs\" 2>/dev/null; done; ",
                "for c in memory cpu pids io; do ",
                "echo \"+$c\" >\"{cg}/cgroup.subtree_control\" 2>/dev/null; done; ",
                "grep -q memory \"{cg}/cgroup.subtree_control\" 2>/dev/null ",
                "|| echo 'WARNING: cgroup memory controller not delegated, pods may fail' >&2; ",
            ),
            cg = cgroup,
            child = child,
        )
    }

    /// Re-exec kubernix inside rootlesskit for rootless operation.
    /// All child processes inherit the user namespace.
    fn reexec_rootless() -> Result<()> {
        info!("Re-executing inside rootlesskit");

        let exe = std::env::current_exe().context("Unable to determine current executable")?;
        let args: Vec<String> = std::env::args().skip(1).collect();

        let kubernix_args = args
            .iter()
            .map(|a| Self::shell_escape(a))
            .collect::<Vec<_>>()
            .join(" ");
        let kubernix_exe = Self::shell_escape(&exe.display().to_string());

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
                "CGRP=/sys/fs/cgroup$(cat /proc/self/cgroup | cut -d: -f3); ",
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
            cmd = Self::shell_escape(&rootlesskit_cmd),
        );

        let mut cmd = Command::new("systemd-run");
        cmd.args([
            "--user",
            "--scope",
            "-p",
            "Delegate=yes",
            "--",
            "bash",
            "-c",
            &outer_cmd,
        ]);
        cmd.env(ROOTLESS_ENV, "1");

        // Replace the process, so that signals reach the session directly.
        // No Kubernix struct or other Drop-bearing resources exist yet,
        // only the logger from prepare_env and the lock, which gets
        // released.
        Err(cmd.exec()).context(
            "Failed to start rootless session. \
             Rootless mode requires systemd-run (with an active user session) \
             and rootlesskit to be installed",
        )
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
    fn bootstrap_cluster(config: Config, lock: Flock<File>) -> Result<()> {
        // Abort into the regular cleanup on termination signals, the
        // components do not receive terminal signals.
        signals::register()?;

        // Remove leftovers of a previous unclean exit
        Self::remove_pid_file(&config);
        if !config.is_rootless() {
            Self::umount_below(config.root(), Duration::ZERO);
        }

        // Setup the progress bar
        const BASE_STEPS: u64 = 15;
        let steps = if config.multi_node() && !config.is_rootless() {
            u64::from(config.nodes()) * 2 + BASE_STEPS
        } else {
            BASE_STEPS
        } + Self::processes(&config);
        let p = Progress::new(steps, config.log_level(), config.log_format());
        info!("Bootstrapping cluster");

        // Ensure that the system is prepared
        let system = System::setup(&config).context("Unable to setup system")?;
        signals::check()?;
        Container::build(&config)?;
        signals::check()?;

        // Setup the network
        let network = Network::new(&config)?;

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
            _system: system,
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
        if let Err(e) = kubernix.write_pid_file().and_then(|()| signals::check()) {
            drop(p);
            return Err(e);
        }
        info!("Everything is up and running");
        p.reset();

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

    fn spawn_addons(&mut self, addon_shutdown: Arc<AtomicBool>) {
        let addon_config = self.config.clone();
        let addon_network = self.network.clone();
        let addon_kubeconfig = self.kubectl.kubeconfig().to_path_buf();
        self.addon_thread = Some(thread::spawn(move || {
            let kubectl = Kubectl::new(&addon_kubeconfig);
            if addon_shutdown.load(Ordering::Relaxed) {
                return;
            }
            if addon_config.addons().iter().any(|a| a == "coredns")
                && let Err(e) = CoreDns::apply(&addon_config, &addon_network, &kubectl)
                && !addon_shutdown.load(Ordering::Relaxed)
            {
                error!("Failed to deploy CoreDNS addon: {}", e);
            }
        }));
    }

    /// Wait until a termination signal occurs or a component dies
    fn wait(&mut self) -> Result<()> {
        info!("Waiting for interrupt…");
        loop {
            if signals::requested() {
                return Ok(());
            }
            if self.processes.iter_mut().flatten().any(|p| p.died()) {
                bail!("A cluster component died unexpectedly, shutting down")
            }
            sleep(Duration::from_millis(100));
        }
    }

    /// Spawn a new interactive default system shell
    fn spawn_shell(&self) -> Result<()> {
        info!("Spawning interactive shell");
        info!("Please be aware that the cluster stops if you exit the shell");

        let mut cmd = Command::new(self.config.shell_ok()?);
        cmd.current_dir(self.config.root());
        Self::apply_env_file(&Self::env_file(&self.config), &mut cmd)?;
        let mut child = cmd.spawn().context("Unable to spawn shell")?;

        loop {
            if child.try_wait()?.is_some() {
                return Ok(());
            }
            // An interrupt from the terminal is meant for the shell, but
            // SIGTERM and SIGHUP stop the cluster.
            signals::clear_interrupt();
            if signals::terminated() {
                info!("Received termination signal, stopping the shell");
                Self::kill_shell(&mut child);
                return Ok(());
            }
            sleep(Duration::from_millis(100));
        }
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
                cmd.env(key, Self::shell_unquote(value));
            }
        }
        Ok(())
    }

    /// Lay out the env file
    fn write_env_file(&self) -> Result<()> {
        info!("Writing environment file");
        fs::write(
            Self::env_file(&self.config),
            Self::env_file_content(&[
                (
                    RUNTIME_ENV,
                    cri::cri_socket(&self.config, &self.network, 0)?.to_socket_string(),
                ),
                (
                    "KUBECONFIG",
                    self.kubectl.kubeconfig().display().to_string(),
                ),
            ]),
        )
        .context("Unable to write environment file")?;
        Ok(())
    }

    /// The env file content for the provided variables, with shell quoted
    /// values.
    fn env_file_content(vars: &[(&str, String)]) -> String {
        vars.iter()
            .map(|(key, value)| format!("export {}={}\n", key, Self::shell_escape(value)))
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
        fs::write(pid_file, id().to_string()).context("Unable to write pid file")
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
    /// container overlays first so remove_dir_all can succeed.
    fn cleanup_rootless_storage(&self) {
        if !self.config.is_rootless() {
            return;
        }
        debug!("Removing rootless CRI storage");
        let root = self.config.root();
        for entry in ["crio", "containerd"] {
            let dir = root.join(entry);
            if !dir.exists() {
                continue;
            }
            Self::umount_below(&dir, Duration::ZERO);
            if let Err(e) = fs::remove_dir_all(&dir) {
                debug!("Unable to remove '{}': {}", dir.display(), e);
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

impl Drop for Kubernix {
    fn drop(&mut self) {
        let p = Progress::new(
            Self::processes(&self.config),
            self.config.log_level(),
            self.config.log_format(),
        );

        info!("Cleaning up");

        self.addon_shutdown.store(true, Ordering::Relaxed);
        if let Some(handle) = self.addon_thread.take() {
            debug!("Waiting for addon deployment to finish");
            let deadline = Instant::now() + Duration::from_secs(5);
            while !handle.is_finished() && Instant::now() < deadline {
                sleep(Duration::from_millis(100));
            }
            if handle.is_finished() {
                let _ = handle.join();
            } else {
                debug!("Addon thread did not finish in time, detaching");
            }
        }

        self.stop();
        self.umount();
        self.cleanup_rootless_storage();
        self.network.cleanup();
        Self::remove_pid_file(&self.config);
        info!("Cleanup done");

        p.reset();
        debug!("All done");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::tempdir;

    #[test]
    fn write_if_changed_creates_new_file() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("test.txt");
        write_if_changed(&path, "hello").unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "hello");
    }

    #[test]
    fn write_if_changed_skips_identical() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("test.txt");
        fs::write(&path, "hello").unwrap();
        let before = fs::metadata(&path).unwrap().modified().unwrap();

        // Small delay so mtime would differ if rewritten
        std::thread::sleep(std::time::Duration::from_millis(10));
        write_if_changed(&path, "hello").unwrap();
        let after = fs::metadata(&path).unwrap().modified().unwrap();
        assert_eq!(before, after);
    }

    #[test]
    fn write_if_changed_updates_different() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("test.txt");
        fs::write(&path, "old").unwrap();
        write_if_changed(&path, "new").unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "new");
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
    fn shell_unquote_inverts_escape() {
        for s in [
            "",
            "plain",
            "a b",
            "it's",
            "'''",
            "\"quoted\"",
            "back\\slash",
        ] {
            assert_eq!(Kubernix::shell_unquote(&Kubernix::shell_escape(s)), s);
        }
        assert_eq!(Kubernix::shell_unquote(r#""a \"b\"""#), r#"a "b""#);
        assert_eq!(Kubernix::shell_unquote(r"a\ b"), "a b");
    }

    #[test]
    fn cgroup_evacuate_snippet() {
        let snippet = Kubernix::cgroup_evacuate("$CGRP", "init");
        assert!(snippet.contains("mkdir -p \"$CGRP/init\"; "));
        assert!(snippet.contains("echo $p >\"$CGRP/init/cgroup.procs\""));
        assert!(snippet.contains(">\"$CGRP/cgroup.subtree_control\""));
        assert!(snippet.ends_with("; "));
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
