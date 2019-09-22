use crate::{config::Config, process::Process};
use failure::Fallible;
use log::debug;

pub struct Crio {
    process: Process,
}

impl Crio {
    pub fn new(config: &Config) -> Fallible<Crio> {
        debug!("Starting CRI-O");
        let mut process = Process::new(config,
        "crio
            --log-level=debug 
            --conmon=/nix/store/9x6hhiv7m8yi58b2891fszv9b999fx34-conmon-2.0.0/bin/conmon
        ")?;

        process.wait_ready("sandboxes:")?;
        debug!("CRI-O is ready");
        Ok(Crio { process })
    }

    pub fn stop(&mut self) -> Fallible<()> {
        self.process.stop()?;
        Ok(())
    }
}
