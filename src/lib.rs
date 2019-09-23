mod config;
mod crio;
mod etcd;
mod kubeconfig;
mod pki;
mod process;

pub use config::Config;

use crio::Crio;
use etcd::Etcd;
use failure::{bail, Fallible};
use kubeconfig::Kubeconfig;
use pki::Pki;

use rayon::scope;
use std::fs::create_dir_all;

pub struct Kubernix {
    etcd: Etcd,
    crio: Crio,
    pki: Pki,
    kubeconfig: Kubeconfig,
}

impl Kubernix {
    pub fn new(config: &Config) -> Fallible<Kubernix> {
        // Setup the PKI
        let pki = Pki::new(config)?;

        // Setup the kubeconfigs
        let kubeconfig = Kubeconfig::new(config, &pki)?;

        // Create the log dir
        create_dir_all(&config.log.dir)?;

        // Spawn the processes
        let mut crio_result: Option<Fallible<Crio>> = None;
        let mut etcd_result: Option<Fallible<Etcd>> = None;
        scope(|s| {
            s.spawn(|_| crio_result = Some(Crio::new(config)));
            s.spawn(|_| etcd_result = Some(Etcd::new(config, &pki)));
        });

        match (crio_result, etcd_result) {
            (Some(c), Some(e)) => {
                return Ok(Kubernix {
                    crio: c?,
                    etcd: e?,
                    pki,
                    kubeconfig,
                })
            }
            _ => bail!("Unable to spawn processes"),
        }
    }

    pub fn stop(&mut self) -> Fallible<()> {
        self.crio.stop()?;
        self.etcd.stop()
    }
}
