//! # kubernix
//!
//! Single dependency, single node Kubernetes clusters for local development
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
mod pki;
mod process;
mod proxy;
mod scheduler;

pub use config::Config;

use apiserver::APIServer;
use controllermanager::ControllerManager;
use coredns::CoreDNS;
use crio::Crio;
use encryptionconfig::EncryptionConfig;
use etcd::Etcd;
use kubeconfig::KubeConfig;
use kubelet::Kubelet;
use pki::Pki;
use process::Startable;
use proxy::Proxy;
use scheduler::Scheduler;

use failure::{bail, format_err, Fallible};
use log::{debug, error, info};
use nix::unistd::getuid;
use rayon::scope;
use std::{
    env::{current_exe, split_paths, var, var_os},
    fmt::Display,
    fs::{self, create_dir_all},
    net::IpAddr,
    path::{Path, PathBuf},
    process::{exit, Command},
};

const RUNTIME_ENV: &str = "CONTAINER_RUNTIME_ENDPOINT";
const KUBECONFIG_ENV: &str = "KUBECONFIG";
const NIX_SHELL_ENV: &str = "IN_NIX_SHELL";

type Stoppables = Vec<Startable>;

/// The main structure for the application
pub struct Kubernix {
    config: Config,
    processes: Stoppables,
    crio_socket: PathBuf,
    kubeconfig: PathBuf,
}

impl Kubernix {
    /// Start kubernix by consuming the provided configuration
    pub fn start(config: Config) -> Fallible<Kubernix> {
        // Rootless is currently not supported
        if !getuid().is_root() {
            bail!("Please run kubernix as root")
        }

        // Bootstrap if we're not inside a nix shell
        if var(NIX_SHELL_ENV).is_err() {
            info!("Nix environment not found, bootstrapping one");
            Self::bootstrap_nix(config)?;
            exit(0);
        } else {
            info!("Bootstrapping cluster inside nix environment");
            Self::bootstrap_cluster(config)
        }
    }

    /// Stop kubernix by cleaning up all running processes
    pub fn stop(&mut self) {
        for x in &mut self.processes {
            if let Err(e) = x.stop() {
                debug!("{}", e)
            }
        }
    }

    fn bootstrap_cluster(config: Config) -> Fallible<Kubernix> {
        // Retrieve the local IP
        let ip = Self::local_ip()?;
        let hostname =
            hostname::get_hostname().ok_or_else(|| format_err!("Unable to retrieve hostname"))?;
        info!("Using local IP {}", ip);

        // Setup the PKI
        let pki = Pki::new(&config, &ip, &hostname)?;

        // Setup the configs
        let kubeconfig = KubeConfig::new(&config, &pki, &ip, &hostname)?;
        let encryptionconfig = EncryptionConfig::new(&config)?;

        // Create the log dir
        create_dir_all(config.root.join(&config.log.dir))?;

        // Spawn the processes
        info!("Starting processes");

        // Full path to the CRI socket
        let crio_socket = config.root.join(&config.crio.dir).join("crio.sock");

        let mut crio = Self::stopped();
        let mut etcd = Self::stopped();
        let mut apis = Self::stopped();
        let mut cont = Self::stopped();
        let mut sche = Self::stopped();
        let mut kube = Self::stopped();
        let mut prox = Self::stopped();

        scope(|s| {
            s.spawn(|_| crio = Crio::start(&config, &crio_socket));
            s.spawn(|_| {
                etcd = Etcd::start(&config, &pki);
                apis = APIServer::start(&config, &ip, &pki, &encryptionconfig, &kubeconfig)
            });
            s.spawn(|_| cont = ControllerManager::start(&config, &pki, &kubeconfig));
            s.spawn(|_| sche = Scheduler::start(&config, &kubeconfig));
            s.spawn(|_| kube = Kubelet::start(&config, &pki, &kubeconfig, &crio_socket));
            s.spawn(|_| prox = Proxy::start(&config, &kubeconfig));
        });

        // Wait for `drain_filter()` to be stable
        let mut started = vec![];
        let mut found_dead = false;

        // This order is important since we will shut down the processes in its reverse order
        for x in vec![kube, sche, prox, cont, apis, etcd, crio] {
            if x.is_ok() {
                started.push(x?)
            } else {
                found_dead = true
            }
        }
        let mut kubernix = Kubernix {
            config: config.clone(),
            processes: started,
            crio_socket,
            kubeconfig: kubeconfig.admin.to_owned(),
        };

        // No dead processes
        if !found_dead {
            CoreDNS::apply(&config, &kubeconfig)?;
            info!("Everything is up and running");

            kubernix.spawn_shell();
            Ok(kubernix)
        } else {
            // Cleanup started processes and exit
            kubernix.stop();
            bail!("Unable to start all processes")
        }
    }

    fn bootstrap_nix(config: Config) -> Fallible<()> {
        // Prepare the nix dir
        let nix_dir = config.root.join("nix");
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
            include_str!("../nix/shell.nix"),
        )?;

        // Run the shell
        let status = Command::new(Self::find_executable("nix-shell")?)
            .arg(nix_dir)
            .arg("--run")
            .arg(current_exe()?)
            .status()?;
        if !status.success() {
            bail!("nix-shell command failed");
        }
        Ok(())
    }

    fn spawn_shell(&self) {
        info!("Spawning interactive shell");
        if let Err(e) = Command::new("bash")
            .current_dir(&self.config.root.join(&self.config.log.dir))
            .arg("--norc")
            .env("PS1", "> ")
            .env(
                RUNTIME_ENV,
                format!("unix://{}", self.crio_socket.display()),
            )
            .env(KUBECONFIG_ENV, &self.kubeconfig)
            .status()
        {
            error!("Unable to spawn shell: {}", e);
        }
    }

    fn stopped<T>() -> Fallible<T> {
        Err(format_err!("Stopped"))
    }

    fn local_ip() -> Fallible<String> {
        let cmd = Command::new("ip")
            .arg("route")
            .arg("get")
            .arg("1.2.3.4")
            .output()?;
        if !cmd.status.success() {
            bail!("Unable to obtain `ip` output")
        }
        let output = String::from_utf8(cmd.stdout)?;
        let ip = output
            .split_whitespace()
            .nth(6)
            .ok_or_else(|| format_err!("Different `ip` command output expected"))?;
        if let Err(e) = ip.parse::<IpAddr>() {
            bail!("Unable to parse IP '{}': {}", ip, e);
        }
        Ok(ip.to_owned())
    }

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
}
