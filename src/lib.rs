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
mod pki;
mod process;
mod proxy;
mod scheduler;

pub use config::{Config, ConfigBuilder};

use apiserver::APIServer;
use controllermanager::ControllerManager;
use coredns::CoreDNS;
use crio::Crio;
use encryptionconfig::EncryptionConfig;
use etcd::Etcd;
use kubeconfig::KubeConfig;
use kubelet::Kubelet;
use pki::Pki;
use process::{Process, Startable};
use proxy::Proxy;
use scheduler::Scheduler;

use env_logger::Builder;
use failure::{bail, format_err, Fallible};
use ipnetwork::IpNetwork;
use log::{debug, error, info};
use nix::unistd::getuid;
use rayon::scope;
use std::{
    env::{current_exe, split_paths, var, var_os},
    fmt::Display,
    fs::{self, create_dir_all},
    net::{IpAddr, Ipv4Addr},
    path::{Path, PathBuf},
    process::Command,
};

const CRIO_DIR: &str = "crio";
const KUBECONFIG_ENV: &str = "KUBECONFIG";
const KUBERNIX_ENV: &str = "kubernix.env";
const LOG_DIR: &str = "log";
const NIX_DIR: &str = "nix";
const NIX_SHELL_ENV: &str = "IN_NIX_SHELL";
const RUNTIME_ENV: &str = "CONTAINER_RUNTIME_ENDPOINT";

type Stoppables = Vec<Startable>;

/// The main entry point for the application
pub struct Kubernix {
    config: Config,
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

        Self::run_nix_shell(
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
        if let Err(e) = CoreDNS::apply(&self.config, &self.kubeconfig) {
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
        fs::write(nix_dir.join("deps.nix"), include_str!("../nix/deps.nix"))?;

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
        Self::run_nix_shell(
            &config,
            &[
                &current_exe()?.display().to_string(),
                "--root",
                &config.root().display().to_string(),
                "--log-level",
                &config.log_level().to_string().to_lowercase(),
                "--crio-cidr",
                &config.crio_cidr().to_string(),
                "--cluster-cidr",
                &config.cluster_cidr().to_string(),
                "--service-cidr",
                &config.service_cidr().to_string(),
            ]
            .join(" "),
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
            .current_dir(self.config.root().join(LOG_DIR))
            .arg("--init-file")
            .arg(env_file)
            .status()?;
        Ok(())
    }

    /// Retrieve the local hosts IP via the default route
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

    /// Run a pure nix shell command
    fn run_nix_shell(config: &Config, arg: &str) -> Fallible<()> {
        Command::new(Self::find_executable("nix-shell")?)
            .arg(config.root().join(NIX_DIR))
            .arg("--pure")
            .arg("-Q")
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

    /// Retrieve the DNS address from the config
    fn dns(config: &Config) -> Fallible<Ipv4Addr> {
        match config.service_cidr() {
            IpNetwork::V4(n) => Ok(n.nth(2).ok_or_else(|| {
                format_err!(
                    "Unable to retrieve second IP from service CIDR: {}",
                    config.service_cidr()
                )
            })?),
            _ => bail!("Service CIDR is not for IPv4"),
        }
    }
}

impl Drop for Kubernix {
    fn drop(&mut self) {
        info!("Cleaning up");
        self.stop();
    }
}
