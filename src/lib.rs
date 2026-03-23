//! # kubernix
#![deny(missing_docs)]

mod apiserver;
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

pub use config::Config;
pub use logger::Logger;

use crate::nix::Nix;
use apiserver::ApiServer;
use container::Container;
use controllermanager::ControllerManager;
use coredns::CoreDns;
use crio::Crio;
use encryptionconfig::EncryptionConfig;
use etcd::Etcd;
use kubeconfig::KubeConfig;
use kubectl::Kubectl;
use kubelet::Kubelet;
use network::Network;
use pki::Pki;
use process::{Process, Stoppables};
use progress::Progress;
use proxy::Proxy;
use scheduler::Scheduler;
use system::System;

use ::nix::{
    mount::{MntFlags, umount2},
    unistd::getuid,
};
use anyhow::{Context, Result, bail};
use log::{debug, error, info, set_boxed_logger};
use rayon::{prelude::*, scope};
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
    thread::sleep,
    time::{Duration, Instant},
};

const RUNTIME_ENV: &str = "CONTAINER_RUNTIME_ENDPOINT";

/// The main entry point for the application
pub struct Kubernix {
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
        set_boxed_logger(Logger::new(config.log_level())).context("Unable to set logger")
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

        // All processes
        info!("Starting processes");
        let mut api_server = Process::stopped();
        let mut controller_manager = Process::stopped();
        let mut etcd = Process::stopped();
        let mut scheduler = Process::stopped();
        let mut proxy = Process::stopped();
        let mut crios = (0..config.nodes())
            .map(|_| Process::stopped())
            .collect::<Vec<_>>();
        let mut kubelets = (0..config.nodes())
            .map(|_| Process::stopped())
            .collect::<Vec<_>>();

        // Spawn the processes
        scope(|a| {
            // Control plane
            a.spawn(|b| {
                etcd = Etcd::start(&config, &network, &pki);
                b.spawn(|c| {
                    api_server =
                        ApiServer::start(&config, &network, &pki, &encryptionconfig, &kubectl);
                    c.spawn(|_| {
                        controller_manager =
                            ControllerManager::start(&config, &network, &pki, &kubeconfig)
                    });
                    c.spawn(|_| scheduler = Scheduler::start(&config, &kubeconfig));
                });
            });

            // Node processes
            a.spawn(|c| {
                crios
                    .par_iter_mut()
                    .zip(kubelets.par_iter_mut())
                    .enumerate()
                    .for_each(|(i, (c, k))| {
                        *c = Crio::start(&config, i as u8, &network);
                        if c.is_ok() {
                            *k = Kubelet::start(&config, i as u8, &network, &pki, &kubeconfig);
                        }
                    });
                c.spawn(|_| proxy = Proxy::start(&config, &network, &kubeconfig));
            });
        });

        // This order is important since we will shut down the processes in order.
        // Kubelets must be stopped before CRI-O because in multi-node mode
        // kubelets run as exec sessions inside CRI-O containers.
        let mut results = vec![scheduler, proxy, controller_manager];
        results.extend(kubelets);
        results.extend(crios);
        results.push(api_server);
        results.push(etcd);
        let all_ok = results.iter().all(|x| x.is_ok());

        // Note: wait for `drain_filter()` to be stable and make it more straightforward
        let mut processes = vec![];
        for process in results {
            match process {
                Ok(p) => processes.push(p),
                Err(e) => debug!("{}", e),
            }
        }

        // Setup the main instance
        let spawn_shell = !config.no_shell();
        let mut kubernix = Kubernix {
            config,
            network,
            kubectl,
            processes,
            system,
        };

        // No dead processes
        if all_ok {
            // Apply all cluster addons
            if let Err(e) = kubernix
                .apply_addons()
                .and_then(|_| kubernix.write_env_file())
            {
                p.reset();
                error!("Unable to start all processes: {}", e);
                return Err(e);
            }
            info!("Everything is up and running");
            p.reset();

            if spawn_shell {
                kubernix.spawn_shell()?;
            } else {
                kubernix.wait()?;
            }
        } else {
            p.reset();
            error!("Unable to start all processes")
        }

        Ok(())
    }

    /// Apply needed workloads to the running cluster. This method stops the cluster on any error.
    fn apply_addons(&mut self) -> Result<()> {
        info!("Applying cluster addons");
        if self.config.addons().iter().any(|a| a == "coredns") {
            CoreDns::apply(&self.config, &self.network, &self.kubectl)?;
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

    /// Parse the env file and apply its variables to a Command
    fn apply_env_file(env_file: &Path, cmd: &mut Command) -> Result<()> {
        let content = fs::read_to_string(env_file)
            .with_context(|| format!("Unable to read env file '{}'", env_file.display()))?;
        for line in content.lines() {
            let line = line.strip_prefix("export ").unwrap_or(line);
            if let Some((key, value)) = line.split_once('=') {
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
        self.stop();
        self.umount();
        self.system.cleanup();
        info!("Cleanup done");

        p.reset();
        debug!("All done");
    }
}
