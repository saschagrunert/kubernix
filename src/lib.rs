//! # kubernix
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
mod system;

pub use config::{Config, CriRuntime, LogFormat, SubCommand};
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
    std::fs::write(path, content)?;
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

/// The port the API server listens on.
pub(crate) const API_SERVER_PORT: u16 = 6443;

use ::nix::{
    mount::{MntFlags, umount2},
    unistd::getuid,
};
use anyhow::{Context, Result, bail};
use log::{debug, error, info, set_boxed_logger};
use signal_hook::{
    consts::signal::{SIGHUP, SIGINT, SIGTERM},
    flag,
};
use std::{
    fs,
    io::{BufRead, BufReader},
    path::{Path, PathBuf},
    process::{Command, id},
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
    processes: Stoppables,
    system: System,
}

impl Kubernix {
    /// Start kubernix by consuming the provided configuration
    pub fn start(mut config: Config) -> Result<()> {
        Self::prepare_env(&mut config)?;

        // Bootstrap if we're not inside a nix shell
        if Nix::is_active() {
            if config.is_rootless() && std::env::var(ROOTLESS_ENV).as_deref() != Ok("1") {
                return Self::reexec_rootless();
            }
            Self::bootstrap_cluster(config)
        } else {
            Nix::bootstrap(config)
        }
    }

    /// Spawn a new shell into the provided configuration environment.
    pub fn new_shell(mut config: Config) -> Result<()> {
        Self::prepare_env(&mut config)?;

        if config.is_rootless() && std::env::var(ROOTLESS_ENV).as_deref() != Ok("1") {
            bail!(
                "The cluster was started in rootless mode. \
                 Run 'kubernix shell' from within the rootlesskit session."
            )
        }

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
            ". '{}' && exec '{}'",
            env_file.display(),
            config.shell_ok()?
        );
        Nix::run(&config, &["bash", "-c", &shell_cmd])?;

        info!("Bye, leaving the Kubernix environment");
        Ok(())
    }

    /// Prepare the environment based on the provided config
    fn prepare_env(config: &mut Config) -> Result<()> {
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

        // Prepare the configuration
        if config.root().exists() {
            config.try_load_file()?;
        } else {
            config.to_file()?;
        }
        config.canonicalize_root()?;

        // Set rootless after config loading since try_load_file
        // replaces the whole struct (resetting #[serde(skip)] fields).
        config.set_rootless(rootless);

        // Setup the logger
        set_boxed_logger(Logger::new(config.log_level(), config.log_format()))
            .context("Unable to set logger")?;

        if config.is_rootless() && !Nix::is_active() {
            info!("Running in rootless mode");
        }

        Ok(())
    }

    /// POSIX single-quote escaping: wraps in single quotes and escapes
    /// any embedded single quotes with the '\'' sequence.
    fn shell_escape(s: &str) -> String {
        format!("'{}'", s.replace('\'', "'\\''"))
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
            concat!(
                "mount -t cgroup2 none /sys/fs/cgroup 2>/dev/null; ",
                "mkdir -p /sys/fs/cgroup/kubernix; ",
                "for p in $(cat /sys/fs/cgroup/cgroup.procs); do ",
                "echo $p >/sys/fs/cgroup/kubernix/cgroup.procs 2>/dev/null; done; ",
                "for c in memory cpu pids io; do ",
                "echo \"+$c\" >/sys/fs/cgroup/cgroup.subtree_control 2>/dev/null; done; ",
                "grep -q memory /sys/fs/cgroup/cgroup.subtree_control 2>/dev/null ",
                "|| echo 'WARNING: cgroup memory controller not delegated, pods may fail' >&2; ",
                "exec {} {}",
            ),
            kubernix_exe, kubernix_args,
        );

        // Evacuate all processes from the scope's root cgroup into a child
        // so we can enable controllers in subtree_control (cgroup v2's
        // "no internal processes" rule forbids it otherwise). This must
        // happen outside rootlesskit's cgroup namespace.
        let outer_cmd = format!(
            concat!(
                "CGRP=/sys/fs/cgroup$(cat /proc/self/cgroup | cut -d: -f3); ",
                "mkdir -p \"$CGRP/init\"; ",
                "for p in $(cat \"$CGRP/cgroup.procs\"); do ",
                "echo $p >\"$CGRP/init/cgroup.procs\" 2>/dev/null; done; ",
                "for c in memory cpu pids io; do ",
                "echo \"+$c\" >\"$CGRP/cgroup.subtree_control\" 2>/dev/null; done; ",
                "grep -q memory \"$CGRP/cgroup.subtree_control\" 2>/dev/null ",
                "|| echo 'WARNING: cgroup memory controller not delegated, pods may fail' >&2; ",
                "exec rootlesskit --net=host --cgroupns ",
                "--copy-up=/etc --copy-up=/run --copy-up=/var/cache ",
                "--copy-up=/var/lib --copy-up=/var/log --copy-up=/var/run ",
                "bash -c {}",
            ),
            Self::shell_escape(&rootlesskit_cmd),
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

        let status = cmd.status().context(
            "Failed to start rootless session. \
             Rootless mode requires systemd-run (with an active user session) \
             and rootlesskit to be installed",
        )?;

        // No Kubernix struct or other Drop-bearing resources exist yet,
        // only the logger from prepare_env.
        std::process::exit(status.code().unwrap_or(1));
    }

    /// Stop kubernix by cleaning up all running processes
    fn stop(&mut self) {
        for x in &mut self.processes {
            if let Err(e) = x.stop() {
                debug!("{}", e)
            }
        }
    }

    /// The amount of processes to be run
    fn processes(config: &Config) -> u64 {
        let base = 4 + 2 * u64::from(config.nodes());
        if config.is_rootless() { base } else { base + 1 }
    }

    /// Bootstrap the whole cluster, which assumes to be inside a nix shell
    fn bootstrap_cluster(config: Config) -> Result<()> {
        // Setup the progress bar
        const BASE_STEPS: u64 = 15;
        let steps = if config.multi_node() && !config.is_rootless() {
            u64::from(config.nodes()) * 2 + BASE_STEPS
        } else {
            BASE_STEPS
        } + Self::processes(&config);
        let p = Progress::new(steps, config.log_level());
        info!("Bootstrapping cluster");

        // Ensure that the system is prepared
        let system = System::setup(&config).context("Unable to setup system")?;
        Container::build(&config)?;

        // Setup the network
        let network = Network::new(&config)?;

        // Setup the public key infrastructure
        let pki = Pki::new(&config, &network)?;

        // Setup the configs
        let kubeconfig = KubeConfig::new(&config, &pki)?;
        let kubectl = Kubectl::new(kubeconfig.admin());
        let encryptionconfig = EncryptionConfig::new(&config)?;

        let ctx = ClusterContext {
            config: &config,
            network: &network,
            pki: &pki,
            kubeconfig: &kubeconfig,
            encryptionconfig: &encryptionconfig,
            kubectl: &kubectl,
        };

        let (processes, all_ok) = Self::register_components(&config).run(&ctx);

        let spawn_shell = !config.no_shell();
        let addon_shutdown = Arc::new(AtomicBool::new(false));
        let mut kubernix = Kubernix {
            addon_shutdown: Arc::clone(&addon_shutdown),
            addon_thread: None,
            config,
            network,
            kubectl,
            processes,
            system,
        };

        if all_ok {
            if let Err(e) = kubernix.write_env_file() {
                p.reset();
                error!("Unable to write environment file: {}", e);
                return Err(e);
            }

            kubernix.spawn_addons(addon_shutdown);
            info!("Everything is up and running");
            p.reset();

            if spawn_shell {
                kubernix.spawn_shell()?;
            } else {
                kubernix.wait()?;
            }
        } else {
            p.reset();
            bail!("Unable to start all processes")
        }

        Ok(())
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
        if !config.is_rootless() {
            registry.register(Box::new(proxy::ProxyComponent));
        }
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

    /// Wait until a termination signal occurs
    fn wait(&self) -> Result<()> {
        // Setup the signal handlers
        let term = Arc::new(AtomicBool::new(false));
        flag::register(SIGTERM, Arc::clone(&term))?;
        flag::register(SIGINT, Arc::clone(&term))?;
        flag::register(SIGHUP, Arc::clone(&term))?;
        info!("Waiting for interrupt…");

        // Write the pid file
        let pid_file = self.config.root().join("kubernix.pid");
        debug!("Writing pid file to: {}", pid_file.display());
        fs::write(pid_file, id().to_string())?;

        // Wait for the signals
        while !term.load(Ordering::Relaxed) {
            sleep(Duration::from_millis(100));
        }
        Ok(())
    }

    /// Spawn a new interactive default system shell
    fn spawn_shell(&self) -> Result<()> {
        info!("Spawning interactive shell");
        info!("Please be aware that the cluster stops if you exit the shell");

        let mut cmd = Command::new(self.config.shell_ok()?);
        cmd.current_dir(self.config.root());
        Self::apply_env_file(&Self::env_file(&self.config), &mut cmd)?;
        cmd.status()?;
        Ok(())
    }

    /// Parse the env file and apply its variables to a Command.
    /// Handles `export KEY=VALUE` and `KEY=VALUE` formats, stripping
    /// surrounding single or double quotes from values.
    fn apply_env_file(env_file: &Path, cmd: &mut Command) -> Result<()> {
        let content = fs::read_to_string(env_file)
            .with_context(|| format!("Unable to read env file '{}'", env_file.display()))?;
        for line in content.lines() {
            let line = line.strip_prefix("export ").unwrap_or(line);
            if let Some((key, value)) = line.split_once('=') {
                let value = value
                    .strip_prefix('"')
                    .and_then(|v| v.strip_suffix('"'))
                    .or_else(|| value.strip_prefix('\'').and_then(|v| v.strip_suffix('\'')))
                    .unwrap_or(value);
                cmd.env(key, value);
            }
        }
        Ok(())
    }

    /// Lay out the env file
    fn write_env_file(&self) -> Result<()> {
        info!("Writing environment file");
        fs::write(
            Self::env_file(&self.config),
            format!(
                "export {}={}\nexport {}={}",
                RUNTIME_ENV,
                cri::cri_socket(&self.config, &self.network, 0)?.to_socket_string(),
                "KUBECONFIG",
                self.kubectl.kubeconfig().display(),
            ),
        )?;
        Ok(())
    }

    /// Retrieve the path to the env file
    fn env_file(config: &Config) -> PathBuf {
        config.root().join("kubernix.env")
    }

    /// Remove all stale mounts
    fn umount(&self) {
        if self.config.is_rootless() {
            debug!("Skipping mount cleanup in rootless mode");
            return;
        }
        debug!("Removing active mounts");
        let now = Instant::now();
        while now.elapsed().as_secs() < 15 {
            match Self::read_mount_points(self.config.root()) {
                Err(e) => {
                    debug!("Unable to retrieve mounts: {}", e);
                    sleep(Duration::from_secs(1));
                }
                Ok(mount_points) => {
                    if mount_points.is_empty() {
                        break;
                    }
                    for dest in &mount_points {
                        debug!("Removing mount: {}", dest.display());
                        if let Err(e) = umount2(dest, MntFlags::MNT_FORCE) {
                            debug!("Unable to umount '{}': {}", dest.display(), e);
                        }
                    }
                    sleep(Duration::from_millis(500));
                }
            };
        }
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
            if let Ok(mounts) = Self::read_mount_points(&dir) {
                for mp in &mounts {
                    if let Err(e) = umount2(mp.as_path(), MntFlags::MNT_DETACH) {
                        debug!("Unable to umount '{}': {}", mp.display(), e);
                    }
                }
            }
            if let Err(e) = fs::remove_dir_all(&dir) {
                debug!("Unable to remove '{}': {}", dir.display(), e);
            }
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
            .filter_map(|line| line.split_whitespace().nth(1).map(PathBuf::from))
            .filter(|p| p.starts_with(root) && p != root)
            .collect();
        points.sort_by_key(|p| std::cmp::Reverse(p.components().count()));
        Ok(points)
    }
}

impl Drop for Kubernix {
    fn drop(&mut self) {
        let p = Progress::new(Self::processes(&self.config), self.config.log_level());

        info!("Cleaning up");

        // Signal the addon thread to stop and give it a short window
        // to finish. If it does not complete in time, proceed with
        // shutdown rather than blocking for up to 120s.
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
                debug!("Addon thread did not finish in time, proceeding with shutdown");
            }
        }

        self.stop();
        self.umount();
        self.cleanup_rootless_storage();
        self.system.cleanup();
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

        let mut cmd = Command::new("echo");
        Kubernix::apply_env_file(&env_file, &mut cmd).unwrap();
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
