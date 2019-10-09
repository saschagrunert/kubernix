//! # kubernix
#![deny(missing_docs)]

mod apiserver;
mod config;
mod controllermanager;
mod coredns;
mod crio;
mod encryptionconfig;
mod etcd;
mod kubeconfig;
mod kubelet;
mod network;
mod nix;
mod pki;
mod process;
mod proxy;
mod scheduler;
mod system;

pub use config::Config;

use crate::nix::Nix;
use apiserver::ApiServer;
use controllermanager::ControllerManager;
use coredns::CoreDNS;
use crio::Crio;
use encryptionconfig::EncryptionConfig;
use etcd::Etcd;
use kubeconfig::KubeConfig;
use kubelet::Kubelet;
use network::Network;
use pki::Pki;
use process::{Process, Startable};
use proxy::Proxy;
use scheduler::Scheduler;
use system::System;

use ::nix::{
    mount::{umount2, MntFlags},
    unistd::getuid,
};
use env_logger::Builder;
use failure::{bail, Fallible};
use log::{debug, error, info};
use proc_mounts::MountIter;
use rayon::scope;
use std::{
    fs,
    process::Command,
    thread::sleep,
    time::{Duration, Instant},
};

const CRIO_DIR: &str = "crio";
const KUBERNIX_ENV: &str = "kubernix.env";
const RUNTIME_ENV: &str = "CONTAINER_RUNTIME_ENDPOINT";

type Stoppables = Vec<Startable>;

/// The main entry point for the application
pub struct Kubernix {
    config: Config,
    network: Network,
    kubeconfig: KubeConfig,
    processes: Stoppables,
}

impl Kubernix {
    /// Start kubernix by consuming the provided configuration
    pub fn start(mut config: Config) -> Fallible<()> {
        Self::prepare_env(&mut config)?;

        // Bootstrap if we're not inside a nix shell
        if Nix::is_active() {
            info!("Bootstrapping cluster inside nix environment");
            Self::bootstrap_cluster(config)
        } else {
            info!("Nix environment not found, bootstrapping one");
            Nix::bootstrap(config)
        }
    }

    /// Spawn a new shell into the provided configuration environment
    pub fn new_shell(mut config: Config) -> Fallible<()> {
        Self::prepare_env(&mut config)?;

        info!(
            "Spawning new kubernix shell in '{}'",
            config.root().display()
        );

        Nix::run(
            &config,
            &[
                &config.shell_ok()?,
                "-c",
                &format!(
                    "source {} && {}",
                    config.root().join(KUBERNIX_ENV).display(),
                    config.shell_ok()?,
                ),
            ],
        )?;

        info!("Bye, leaving the Kubernix environment");
        Ok(())
    }

    /// Prepare the environment based on the provided config
    fn prepare_env(config: &mut Config) -> Fallible<()> {
        // Rootless is currently not supported
        if !getuid().is_root() {
            bail!("Please run kubernix as root")
        }

        // Prepare the configuration
        if config.root().exists() {
            config.update_from_file()?;
        } else {
            config.to_file()?;
        }
        config.canonicalize_root()?;

        // Setup the logger
        let mut builder = Builder::new();
        builder
            .format_timestamp(None)
            .filter(None, *config.log_level())
            .try_init()?;

        Ok(())
    }

    /// Stop kubernix by cleaning up all running processes
    fn stop(&mut self) {
        for x in &mut self.processes {
            if let Err(e) = x.stop() {
                debug!("{}", e)
            }
        }
    }

    /// Bootstrap the whole cluster, which assumes to be inside a nix shell
    fn bootstrap_cluster(config: Config) -> Fallible<()> {
        // Ensure that the system is prepared
        let system = System::new()?;

        // Setup the network
        let network = Network::new(&config)?;

        // Setup the public key infrastructure
        let pki = Pki::new(&config, &system, &network)?;

        // Setup the configs
        let kubeconfig = KubeConfig::new(&config, &system, &pki)?;
        let encryptionconfig = EncryptionConfig::new(&config)?;

        // All processes
        let mut crio = Process::stopped();
        let mut etcd = Process::stopped();
        let mut api_server = Process::stopped();
        let mut controller_manager = Process::stopped();
        let mut scheduler = Process::stopped();
        let mut kubelet = Process::stopped();
        let mut proxy = Process::stopped();

        // Spawn the processes
        info!("Starting processes");
        scope(|s| {
            s.spawn(|_| crio = Crio::start(&config, &network));
            s.spawn(|_| {
                etcd = Etcd::start(&config, &network, &pki);
                api_server = ApiServer::start(
                    &config,
                    &system,
                    &network,
                    &pki,
                    &encryptionconfig,
                    &kubeconfig,
                )
            });
            s.spawn(|_| {
                controller_manager = ControllerManager::start(&config, &network, &pki, &kubeconfig)
            });
            s.spawn(|_| scheduler = Scheduler::start(&config, &kubeconfig));
            s.spawn(|_| kubelet = Kubelet::start(&config, &network, &pki, &kubeconfig));
            s.spawn(|_| proxy = Proxy::start(&config, &network, &kubeconfig));
        });

        let mut processes = vec![];

        // This order is important since we will shut down the processes in order
        let results = vec![
            kubelet,
            scheduler,
            proxy,
            controller_manager,
            api_server,
            etcd,
            crio,
        ];
        let all_ok = results.iter().all(|x| x.is_ok());

        // Note: wait for `drain_filter()` to be stable and make it more straightforward
        for process in results {
            match process {
                Ok(p) => processes.push(p),
                Err(e) => error!("{}", e),
            }
        }

        // Setup the main instance
        let mut kubernix = Kubernix {
            config,
            network,
            kubeconfig,
            processes,
        };

        // No dead processes
        if all_ok {
            kubernix.apply_addons()?;

            info!("Everything is up and running");
            kubernix.spawn_shell()?;
        } else {
            error!("Unable to start all processes")
        }

        Ok(())
    }

    /// Apply needed workloads to the running cluster. This method stops the cluster on any error.
    fn apply_addons(&mut self) -> Fallible<()> {
        if let Err(e) = CoreDNS::apply(&self.config, &self.network, &self.kubeconfig) {
            bail!("Unable to apply CoreDNS: {}", e);
        }
        Ok(())
    }

    /// Spawn a new interactive default system shell
    fn spawn_shell(&self) -> Fallible<()> {
        info!("Spawning interactive shell");
        info!("Please be aware that the cluster gets destroyed if you exit the shell");
        let env_file = self.config.root().join(KUBERNIX_ENV);
        fs::write(
            &env_file,
            format!(
                "export {}={}\nexport {}={}",
                RUNTIME_ENV,
                self.network.crio_socket().to_socket_string(),
                "KUBECONFIG",
                self.kubeconfig.admin().display(),
            ),
        )?;

        Command::new(self.config.shell_ok()?)
            .current_dir(self.config.root())
            .arg("-c")
            .arg(format!(
                "source {} && {}",
                env_file.display(),
                self.config.shell_ok()?,
            ))
            .status()?;
        Ok(())
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
        info!("Cleaning up");
        self.stop();
        self.umount();
    }
}
