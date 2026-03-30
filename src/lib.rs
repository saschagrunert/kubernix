//! # kubernix
#![deny(missing_docs)]

mod apiserver;
mod component;
mod config;
mod container;
mod controllermanager;
mod coredns;
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

pub use config::{Config, LogFormat};
pub use logger::Logger;

/// Write `content` to `path` only if the file does not exist or its
/// current contents differ. Avoids unnecessary filesystem writes and
/// inode updates on warm restarts.
pub(crate) fn write_if_changed(path: &std::path::Path, content: &str) -> anyhow::Result<()> {
    if path.exists() {
        if let Ok(existing) = std::fs::read_to_string(path) {
            if existing == content {
                return Ok(());
            }
        }
    }
    std::fs::write(path, content)?;
    Ok(())
}

use crate::nix::Nix;
use component::{ClusterContext, ComponentRegistry};
use container::Container;
use coredns::CoreDns;
use crio::Crio;
use encryptionconfig::EncryptionConfig;
use kubeconfig::KubeConfig;
use kubectl::Kubectl;
use network::Network;
use pki::Pki;
use process::Stoppables;
use progress::Progress;
use system::System;

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

const RUNTIME_ENV: &str = "CONTAINER_RUNTIME_ENDPOINT";

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
            Self::bootstrap_cluster(config)
        } else {
            Nix::bootstrap(config)
        }
    }

    /// Spawn a new shell into the provided configuration environment
    pub fn new_shell(mut config: Config) -> Result<()> {
        Self::prepare_env(&mut config)?;

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

        let shell_cmd = format!(". {} && exec {}", env_file.display(), config.shell_ok()?);
        Nix::run(&config, &["bash", "-c", &format!("'{}'", shell_cmd)])?;

        info!("Bye, leaving the Kubernix environment");
        Ok(())
    }

    /// Prepare the environment based on the provided config
    fn prepare_env(config: &mut Config) -> Result<()> {
        // Rootless is currently not supported
        if !getuid().is_root() {
            bail!("Please run kubernix as root")
        }

        // Prepare the configuration
        if config.root().exists() {
            config.try_load_file()?;
        } else {
            config.to_file()?;
        }
        config.canonicalize_root()?;

        // Setup the logger
        set_boxed_logger(Logger::new(config.log_level(), config.log_format()))
            .context("Unable to set logger")
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
        5 + 2 * u64::from(config.nodes())
    }

    /// Bootstrap the whole cluster, which assumes to be inside a nix shell
    fn bootstrap_cluster(config: Config) -> Result<()> {
        // Setup the progress bar
        const BASE_STEPS: u64 = 15;
        let steps = if config.multi_node() {
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

        // Build the component registry
        let ctx = ClusterContext {
            config: &config,
            network: &network,
            pki: &pki,
            kubeconfig: &kubeconfig,
            encryptionconfig: &encryptionconfig,
            kubectl: &kubectl,
        };

        let mut registry = ComponentRegistry::new();
        registry.register(Box::new(etcd::EtcdComponent));
        registry.register(Box::new(apiserver::ApiServerComponent));
        registry.register(Box::new(controllermanager::ControllerManagerComponent));
        registry.register(Box::new(scheduler::SchedulerComponent));
        for node in 0..config.nodes() {
            registry.register(Box::new(crio::CrioComponent::new(node)));
            registry.register(Box::new(kubelet::KubeletComponent::new(node)));
        }
        registry.register(Box::new(proxy::ProxyComponent));

        let (processes, all_ok) = registry.run(&ctx);

        // Setup the main instance
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

        // No dead processes
        if all_ok {
            if let Err(e) = kubernix.write_env_file() {
                p.reset();
                error!("Unable to write environment file: {}", e);
                return Err(e);
            }

            // Deploy addons asynchronously so the shell is available
            // sooner. Failures are logged but do not block startup.
            // The thread handle is stored so Drop can join it before
            // tearing down cluster processes.
            let addon_config = kubernix.config.clone();
            let addon_network = kubernix.network.clone();
            let addon_kubeconfig = kubernix.kubectl.kubeconfig().to_path_buf();
            kubernix.addon_thread = Some(thread::spawn(move || {
                let kubectl = Kubectl::new(&addon_kubeconfig);
                if addon_shutdown.load(Ordering::Relaxed) {
                    return;
                }
                if addon_config.addons().iter().any(|a| a == "coredns") {
                    if let Err(e) = CoreDns::apply(&addon_config, &addon_network, &kubectl) {
                        if !addon_shutdown.load(Ordering::Relaxed) {
                            error!("Failed to deploy CoreDNS addon: {}", e);
                        }
                    }
                }
            }));

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
                Crio::socket(&self.config, &self.network, 0)?.to_socket_string(),
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
        self.system.cleanup();
        info!("Cleanup done");

        p.reset();
        debug!("All done");
    }
}
