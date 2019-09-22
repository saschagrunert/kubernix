mod config;
mod crio;
mod etcd;
mod process;

pub use config::Config;

use crio::Crio;
use etcd::Etcd;
use failure::Fallible;

use std::fs::create_dir_all;

pub struct Kubernix {
    etcd: Etcd,
    crio: Crio,
}

impl Kubernix {
    pub fn new(config: &Config) -> Fallible<Kubernix> {
        // Create the log dir
        create_dir_all(&config.log.dir)?;

        // Spawn the processes
        let etcd = Etcd::new(config)?;
        let crio = Crio::new(config)?;
        Ok(Kubernix { crio, etcd })
    }

    pub fn stop(&mut self) -> Fallible<()> {
        self.crio.stop()?;
        self.etcd.stop()
    }
}
