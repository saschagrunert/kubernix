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

const ASSETS_DIR: &str = "assets";

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
        let mut crio_result: Option<Fallible<Crio>> = None;
        let mut etcd_result: Option<Fallible<Etcd>> = None;
        let mut apiserver_result: Option<Fallible<APIServer>> = None;
        let mut controllermanager_result: Option<Fallible<ControllerManager>> =
            None;
        let mut scheduler_result: Option<Fallible<Scheduler>> = None;
        let mut kubelet_result: Option<Fallible<Kubelet>> = None;
        let mut proxy_result: Option<Fallible<Proxy>> = None;

        // Full path to the CRI socket
        let socket = config.root.join(&config.crio.dir).join("crio.sock");

        scope(|s| {
            s.spawn(|_| crio_result = Some(Crio::new(config, &socket)));
            s.spawn(|_| {
                etcd_result = Some(Etcd::new(config, &pki));
                apiserver_result = Some(APIServer::new(
                    config,
                    &ip,
                    &pki,
                    &encryptionconfig,
                    &kubeconfig,
                ));
            });
            s.spawn(|_| {
                controllermanager_result =
                    Some(ControllerManager::new(config, &pki, &kubeconfig))
            });
            s.spawn(|_| {
                scheduler_result = Some(Scheduler::new(config, &kubeconfig))
            });
            s.spawn(|_| {
                kubelet_result =
                    Some(Kubelet::new(config, &pki, &kubeconfig, &socket))
            });
            s.spawn(|_| proxy_result = Some(Proxy::new(config, &kubeconfig)));
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
                Some(Ok(crio)),
                Some(Ok(etcd)),
                Some(Ok(apiserver)),
                Some(Ok(controllermanager)),
                Some(Ok(scheduler)),
                Some(Ok(kubelet)),
                Some(Ok(proxy)),
            ) => {
                CoreDNS::apply(&config, &kubeconfig)?;

                info!("Everything is up and running");
                Ok(Kubernix {
                    processes: vec![
                        Box::new(crio),
                        Box::new(etcd),
                        Box::new(apiserver),
                        Box::new(controllermanager),
                        Box::new(scheduler),
                        Box::new(kubelet),
                        Box::new(proxy),
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
                if let Some(Ok(mut x)) = crio {
                    x.stop();
                }
                if let Some(Ok(mut x)) = etcd {
                    x.stop();
                }
                if let Some(Ok(mut x)) = apiserver {
                    x.stop();
                }
                if let Some(Ok(mut x)) = controllermanager {
                    x.stop();
                }
                if let Some(Ok(mut x)) = scheduler {
                    x.stop();
                }
                if let Some(Ok(mut x)) = kubelet {
                    x.stop();
                }
                if let Some(Ok(mut x)) = proxy {
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
