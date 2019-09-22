use crate::{config::Config, process::Process};
use failure::Fallible;
use log::debug;

pub struct Etcd {
    process: Process,
}

impl Etcd {
    pub fn new(config: &Config) -> Fallible<Etcd> {
        debug!("Starting etcd");
        let mut process = Process::new(config, "etcd")?;

        process.wait_ready("ready to serve client requests")?;
        debug!("etcd is ready");
        Ok(Etcd { process })
    }

    pub fn stop(&mut self) -> Fallible<()> {
        self.process.stop()?;
        Ok(())
    }
}
