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
use process::Stoppable;
use proxy::Proxy;
use scheduler::Scheduler;

use failure::{bail, format_err, Fallible};
use log::info;
use rayon::scope;
use std::{fs::create_dir_all, process::Command};

const LOCALHOST: &str = "127.0.0.1";

pub struct Kubernix {
    processes: Vec<Box<dyn Stoppable>>,
}

impl Kubernix {
    pub fn new(config: &Config) -> Fallible<Kubernix> {
        // Retrieve the local IP
        let ip = Self::local_ip()?;
        let hostname = hostname::get_hostname()
            .ok_or_else(|| format_err!("Unable to retrieve hostname"))?;
        info!("Using local IP {}", ip);

        // Setup the PKI
        let pki = Pki::new(config, &ip, &hostname)?;

        // Setup the configs
        let kubeconfig = KubeConfig::new(config, &pki, &ip, &hostname)?;
        let encryptionconfig = EncryptionConfig::new(config)?;

        // Create the log dir
        create_dir_all(config.root.join(&config.log.dir))?;

        // Spawn the processes
        info!("Starting processes");
        let mut crio_result: Fallible<Crio> = Err(format_err!("Not started"));
        let mut etcd_result: Fallible<Etcd> = Err(format_err!("Not started"));
        let mut apiserver_result: Fallible<APIServer> =
            Err(format_err!("Not started"));
        let mut controllermanager_result: Fallible<ControllerManager> =
            Err(format_err!("Not started"));
        let mut scheduler_result: Fallible<Scheduler> =
            Err(format_err!("Not started"));
        let mut kubelet_result: Fallible<Kubelet> =
            Err(format_err!("Not started"));
        let mut proxy_result: Fallible<Proxy> = Err(format_err!("Not started"));

        // Full path to the CRI socket
        let socket = config.root.join(&config.crio.dir).join("crio.sock");

        scope(|s| {
            s.spawn(|_| crio_result = Crio::new(config, &socket));
            s.spawn(|_| {
                etcd_result = Etcd::new(config, &pki);
                apiserver_result = APIServer::new(
                    config,
                    &ip,
                    &pki,
                    &encryptionconfig,
                    &kubeconfig,
                );
            });
            s.spawn(|_| {
                controllermanager_result =
                    ControllerManager::new(config, &pki, &kubeconfig)
            });
            s.spawn(|_| scheduler_result = Scheduler::new(config, &kubeconfig));
            s.spawn(|_| {
                kubelet_result =
                    Kubelet::new(config, &pki, &kubeconfig, &socket)
            });
            s.spawn(|_| proxy_result = Proxy::new(config, &kubeconfig));
        });

        match (
            crio_result,
            etcd_result,
            apiserver_result,
            controllermanager_result,
            scheduler_result,
            kubelet_result,
            proxy_result,
        ) {
            (
                Ok(crio),
                Ok(etcd),
                Ok(apiserver),
                Ok(controllermanager),
                Ok(scheduler),
                Ok(kubelet),
                Ok(proxy),
            ) => {
                CoreDNS::apply(&config, &kubeconfig)?;

                info!("Everything is up and running");
                Ok(Kubernix {
                    processes: vec![
                        Box::new(kubelet),
                        Box::new(proxy),
                        Box::new(apiserver),
                        Box::new(controllermanager),
                        Box::new(scheduler),
                        Box::new(etcd),
                        Box::new(crio),
                    ],
                })
            }
            (
                crio,
                etcd,
                apiserver,
                controllermanager,
                scheduler,
                kubelet,
                proxy,
            ) => {
                if let Ok(mut x) = crio {
                    x.stop();
                }
                if let Ok(mut x) = etcd {
                    x.stop();
                }
                if let Ok(mut x) = apiserver {
                    x.stop();
                }
                if let Ok(mut x) = controllermanager {
                    x.stop();
                }
                if let Ok(mut x) = scheduler {
                    x.stop();
                }
                if let Ok(mut x) = kubelet {
                    x.stop();
                }
                if let Ok(mut x) = proxy {
                    x.stop();
                }

                bail!("Unable to spawn processes")
            }
        }
    }

    pub fn stop(&mut self) {
        for x in &mut self.processes {
            x.stop();
        }
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
        let ip = out.split_whitespace().nth(6).ok_or_else(|| {
            format_err!("Different `ip` command output expected")
        })?;
        Ok(ip.to_owned())
    }
}
