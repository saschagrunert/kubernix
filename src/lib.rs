mod apiserver;
mod config;
mod controllermanager;
mod crio;
mod encryptionconfig;
mod etcd;
mod kubeconfig;
mod pki;
mod process;

pub use config::Config;

use apiserver::APIServer;
use controllermanager::ControllerManager;
use crio::Crio;
use encryptionconfig::EncryptionConfig;
use etcd::Etcd;
use failure::{bail, Fallible};
use kubeconfig::KubeConfig;
use pki::Pki;

use failure::format_err;
use log::info;
use rayon::scope;
use std::{fs::create_dir_all, process::Command};

const ASSETS_DIR: &str = "assets";

pub struct Kubernix {
    pki: Pki,
    kubeconfig: KubeConfig,
    encryptionconfig: EncryptionConfig,
    etcd: Etcd,
    crio: Crio,
    apiserver: APIServer,
    controllermanager: ControllerManager,
}

impl Kubernix {
    pub fn new(config: &Config) -> Fallible<Kubernix> {
        // Retrieve the local IP
        let ip = Self::local_ip()?;
        info!("Using local IP {}", ip);

        // Setup the PKI
        let pki = Pki::new(config, &ip)?;

        // Setup the configs
        let kubeconfig = KubeConfig::new(config, &pki)?;
        let encryptionconfig = EncryptionConfig::new(config)?;

        // Create the log dir
        create_dir_all(config.root.join(&config.log.dir))?;

        // Spawn the processes
        info!("Starting processes");
        let mut crio_result: Option<Fallible<Crio>> = None;
        let mut etcd_result: Option<Fallible<Etcd>> = None;
        scope(|s| {
            s.spawn(|_| crio_result = Some(Crio::new(config)));
            s.spawn(|_| etcd_result = Some(Etcd::new(config, &pki)));
        });

        let apiserver = APIServer::new(config, &ip, &pki, &encryptionconfig)?;
        let controllermanager =
            ControllerManager::new(config, &pki, &kubeconfig)?;

        match (crio_result, etcd_result) {
            (Some(c), Some(e)) => {
                return Ok(Kubernix {
                    pki,
                    kubeconfig,
                    encryptionconfig,
                    crio: c?,
                    etcd: e?,
                    apiserver,
                    controllermanager,
                })
            }
            _ => bail!("Unable to spawn processes"),
        }
    }

    pub fn stop(&mut self) -> Fallible<()> {
        self.apiserver.stop()?;
        self.controllermanager.stop()?;
        self.crio.stop()?;
        self.etcd.stop()
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
            format_err!("Different ip command output expected")
        })?;
        Ok(ip.to_owned())
    }
}
