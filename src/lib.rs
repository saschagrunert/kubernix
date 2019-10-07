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
mod pki;
mod process;
mod proxy;
mod scheduler;
mod system;

pub use config::Config;

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

use env_logger::Builder;
use failure::{bail, format_err, Fallible};
use log::{debug, error, info, LevelFilter};
use nix::{
    mount::{umount2, MntFlags},
    unistd::getuid,
};
use proc_mounts::MountIter;
use rayon::scope;
use std::{
    env::{current_exe, split_paths, var, var_os},
    fmt::Display,
    fs::{self, create_dir_all},
    path::{Path, PathBuf},
    process::Command,
    thread::sleep,
    time::{Duration, Instant},
};

const CRIO_DIR: &str = "crio";
const NIX_DIR: &str = "nix";
const KUBERNIX_ENV: &str = "kubernix.env";

const KUBECONFIG_ENV: &str = "KUBECONFIG";
const NIX_SHELL_ENV: &str = "IN_NIX_SHELL";
const RUNTIME_ENV: &str = "CONTAINER_RUNTIME_ENDPOINT";

type Stoppables = Vec<Startable>;

/// The main entry point for the application
pub struct Kubernix {
    config: Config,
    network: Network,
    crio_socket: PathBuf,
    kubeconfig: KubeConfig,
    processes: Stoppables,
}

impl Kubernix {
    /// Start kubernix by consuming the provided configuration
    pub fn start(mut config: Config) -> Fallible<()> {
        Self::prepare_env(&mut config)?;

        // Bootstrap if we're not inside a nix shell
        if var(NIX_SHELL_ENV).is_err() {
            info!("Nix environment not found, bootstrapping one");
            Self::bootstrap_nix(config)
        } else {
            info!("Bootstrapping cluster inside nix environment");
            Self::bootstrap_cluster(config)
        }
    }

    /// Spawn a new shell into the provided configuration environment
    pub fn new_shell(mut config: Config) -> Fallible<()> {
        Self::prepare_env(&mut config)?;

        info!(
            "Spawning new kubernix shell in '{}'",
            config.root().display()
        );

        Self::nix_shell_run(
            &config,
            &format!(
                "bash --init-file {}",
                config.root().join(KUBERNIX_ENV).display()
            ),
        )
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
        let system = System::new();
        system.prepare()?;

        // Retrieve the local IP
        let ip = system.ip()?;
        let hostname = system.hostname()?;

        // Setup the network
        let network = Network::new(&config)?;

        // Setup the PKI
        let pki = Pki::new(&config, &network, &ip, &hostname)?;

        // Setup the configs
        let kubeconfig = KubeConfig::new(&config, &pki, &ip, &hostname)?;
        let encryptionconfig = EncryptionConfig::new(&config)?;

        // Full path to the CRI socket
        let crio_socket = config.root().join(CRIO_DIR).join("crio.sock");

        // All processes
        let mut crio = Process::stopped();
        let mut etcd = Process::stopped();
        let mut apis = Process::stopped();
        let mut cont = Process::stopped();
        let mut sche = Process::stopped();
        let mut kube = Process::stopped();
        let mut prox = Process::stopped();

        // Spawn the processes
        info!("Starting processes");
        scope(|s| {
            s.spawn(|_| crio = Crio::start(&config, &network, &crio_socket));
            s.spawn(|_| {
                etcd = Etcd::start(&config, &pki);
                apis =
                    ApiServer::start(&config, &network, &ip, &pki, &encryptionconfig, &kubeconfig)
            });
            s.spawn(|_| cont = ControllerManager::start(&config, &network, &pki, &kubeconfig));
            s.spawn(|_| sche = Scheduler::start(&config, &kubeconfig));
            s.spawn(|_| kube = Kubelet::start(&config, &network, &pki, &kubeconfig, &crio_socket));
            s.spawn(|_| prox = Proxy::start(&config, &network, &kubeconfig));
        });

        let mut processes = vec![];

        // This order is important since we will shut down the processes in its reverse
        let results = vec![kube, sche, prox, cont, apis, etcd, crio];
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
            crio_socket,
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

    /// Bootstrap the nix environment
    fn bootstrap_nix(config: Config) -> Fallible<()> {
        // Prepare the nix dir
        let nix_dir = config.root().join(NIX_DIR);
        create_dir_all(&nix_dir)?;

        // Write the configuration
        fs::write(
            nix_dir.join("nixpkgs.json"),
            include_str!("../nix/nixpkgs.json"),
        )?;
        fs::write(
            nix_dir.join("nixpkgs.nix"),
            include_str!("../nix/nixpkgs.nix"),
        )?;
        fs::write(
            nix_dir.join("default.nix"),
            include_str!("../nix/default.nix"),
        )?;

        let packages = &config.packages().join(" ");
        debug!("Adding additional packages: {}", packages);
        fs::write(
            nix_dir.join("deps.nix"),
            include_str!("../nix/deps.nix").replace("/* PACKAGES */", packages),
        )?;

        // Apply the overlay if existing
        let target_overlay = nix_dir.join("overlay.nix");
        match config.overlay() {
            // User defined overlay
            Some(overlay) => {
                info!("Using custom overlay '{}'", overlay.display());
                fs::copy(overlay, target_overlay)?;
            }

            // The default overlay
            None => {
                debug!("Using default overlay");
                fs::write(target_overlay, include_str!("../nix/overlay.nix"))?;
            }
        }

        // Run the shell
        Self::nix_shell_run(
            &config,
            &format!(
                "{} --root {}",
                current_exe()?.display(),
                config.root().display()
            ),
        )
    }

    /// Spawn a new interactive nix shell
    fn spawn_shell(&self) -> Fallible<()> {
        info!("Spawning interactive shell");
        info!("Please be aware that the cluster gets destroyed if you exit the shell");
        let env_file = self.config.root().join(KUBERNIX_ENV);
        fs::write(
            &env_file,
            format!(
                "PS1='> '\nexport {}={}\nexport {}={}",
                RUNTIME_ENV,
                format!("unix://{}", self.crio_socket.display()),
                KUBECONFIG_ENV,
                self.kubeconfig.admin().display(),
            ),
        )?;

        Command::new("bash")
            .current_dir(self.config.root())
            .arg("--init-file")
            .arg(env_file)
            .status()?;
        Ok(())
    }

    /// Run a pure nix shell command
    fn nix_shell_run(config: &Config, arg: &str) -> Fallible<()> {
        let purity = if !*config.impure() {
            debug!("Runnig pure nix-shell");
            "--pure"
        } else {
            info!("Runnig impure nix-shell");
            "--impure"
        };
        let verbosity = match *config.log_level() {
            LevelFilter::Trace => "-vvvvv",
            LevelFilter::Debug => "--verbose",
            LevelFilter::Info => "-Q", // just no build output
            _ => "--quiet",
        };
        Command::new(Self::find_executable("nix-shell")?)
            .arg(config.root().join(NIX_DIR))
            .arg(purity)
            .arg(verbosity)
            .arg(format!("-j{}", num_cpus::get()))
            .arg("--run")
            .arg(arg)
            .status()?;
        Ok(())
    }

    /// Find an executable inside the current $PATH environment
    fn find_executable<P>(name: P) -> Fallible<PathBuf>
    where
        P: AsRef<Path> + Display,
    {
        var_os("PATH")
            .and_then(|paths| {
                split_paths(&paths)
                    .filter_map(|dir| {
                        let full_path = dir.join(&name);
                        if full_path.is_file() {
                            Some(full_path)
                        } else {
                            None
                        }
                    })
                    .next()
            })
            .ok_or_else(|| format_err!("Unable to find {} in $PATH", name))
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
