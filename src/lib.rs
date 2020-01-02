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
use coredns::CoreDNS;
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
    mount::{umount2, MntFlags},
    unistd::getuid,
};
use anyhow::{bail, Context, Result};
use log::{debug, error, info, set_boxed_logger};
use proc_mounts::MountIter;
use rayon::{prelude::*, scope};
use signal_hook::{flag, SIGHUP, SIGINT, SIGTERM};
use std::{
    fs,
    path::PathBuf,
    process::{id, Command},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
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

        Nix::run(
            &config,
            &[
                &config.shell_ok()?,
                "-c",
                &format!(". {} && {}", env_file.display(), config.shell_ok()?,),
            ],
        )?;

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

        // This order is important since we will shut down the processes in order
        let mut results = vec![scheduler, proxy, controller_manager, api_server, etcd];
        results.extend(kubelets);
        results.extend(crios);
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
            kubernix.apply_addons()?;
            kubernix.write_env_file()?;
            info!("Everything is up and running");
            p.reset();

            if spawn_shell {
                kubernix.spawn_shell()?;
            } else {
                kubernix.wait()?;
            }
        } else {
            error!("Unable to start all processes")
        }

        Ok(())
    }

    /// Apply needed workloads to the running cluster. This method stops the cluster on any error.
    fn apply_addons(&mut self) -> Result<()> {
        info!("Applying cluster addons");
        CoreDNS::apply(&self.config, &self.network, &self.kubectl)
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
        while !term.load(Ordering::Relaxed) {}
        Ok(())
    }

    /// Spawn a new interactive default system shell
    fn spawn_shell(&self) -> Result<()> {
        info!("Spawning interactive shell");
        info!("Please be aware that the cluster stops if you exit the shell");

        Command::new(self.config.shell_ok()?)
            .current_dir(self.config.root())
            .arg("-c")
            .arg(format!(
                ". {} && {}",
                Self::env_file(&self.config).display(),
                self.config.shell_ok()?,
            ))
            .status()?;
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
        while now.elapsed().as_secs() < 5 {
            match MountIter::new() {
                Err(e) => {
                    debug!("Unable to retrieve mounts: {}", e);
                    sleep(Duration::from_secs(1));
                }
                Ok(mounts) => {
                    let mut found_mount = false;
                    mounts
                        .filter_map(|x| x.ok())
                        .filter(|x| x.dest.starts_with(self.config.root()))
                        .filter(|x| !x.dest.eq(self.config.root()))
                        .for_each(|m| {
                            found_mount = true;
                            debug!("Removing mount: {}", m.dest.display());
                            if let Err(e) = umount2(&m.dest, MntFlags::MNT_FORCE) {
                                debug!("Unable to umount '{}': {}", m.dest.display(), e);
                            }
                        });
                    if !found_mount {
                        break;
                    }
                }
            };
        }
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
