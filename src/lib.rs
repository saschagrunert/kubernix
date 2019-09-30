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
use rayon::scope;
use std::{fs::create_dir_all, process::Command};

const LOCALHOST: &str = "127.0.0.1";

type Stoppables = Vec<Startable>;

pub struct Kubernix {
    config: Config,
    processes: Stoppables,
}

impl Kubernix {
    pub fn new(config: Config) -> Fallible<Kubernix> {
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
        let socket = config.root.join(&config.crio.dir).join("crio.sock");

        let mut crio = Self::stopped();
        let mut etcd = Self::stopped();
        let mut apis = Self::stopped();
        let mut cont = Self::stopped();
        let mut sche = Self::stopped();
        let mut kube = Self::stopped();
        let mut prox = Self::stopped();

        scope(|s| {
            s.spawn(|_| crio = Crio::start(&config, &socket));
            s.spawn(|_| {
                etcd = Etcd::start(&config, &pki);
                apis = APIServer::start(&config, &ip, &pki, &encryptionconfig, &kubeconfig)
            });
            s.spawn(|_| cont = ControllerManager::start(&config, &pki, &kubeconfig));
            s.spawn(|_| sche = Scheduler::start(&config, &kubeconfig));
            s.spawn(|_| kube = Kubelet::start(&config, &pki, &kubeconfig, &socket));
            s.spawn(|_| prox = Proxy::start(&config, &kubeconfig));
        });

        // Wait for `drain_filter()` to be stable
        let mut started = vec![];
        let mut found_dead = false;

        // This order is important since we will shut down the processes in its reverse order
        for x in vec![sche, prox, cont, apis, etcd, crio] {
            if x.is_ok() {
                started.push(x?)
            } else {
                found_dead = true
            }
        }
        let mut kubernix = Kubernix {
            config: config.clone(),
            processes: started,
        };

        // No dead processes
        if !found_dead {
            CoreDNS::apply(&config, &kubeconfig)?;

            info!("Everything is up and running");
            Ok(kubernix)
        } else {
            // Cleanup started processes and exit
            kubernix.stop();
            bail!("Unable to start all processes")
        }
    }

    pub fn shell(&self) {
        if let Err(e) = Command::new("bash")
            .current_dir(&self.config.root.join(&self.config.log.dir))
            .status()
        {
            error!("Unable to spawn shell: {}", e);
        }
    }

    pub fn stop(&mut self) {
        for x in &mut self.processes {
            if let Err(e) = x.stop() {
                debug!("{}", e)
            }
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
            bail!("unable to retrieve local IP")
        }
        let out = String::from_utf8(cmd.stdout)?;
        let ip = out
            .split_whitespace()
            .nth(6)
            .ok_or_else(|| format_err!("Different `ip` command output expected"))?;
        Ok(ip.to_owned())
    }
}
