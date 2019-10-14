use crate::{
    config::Config,
    kubeconfig::KubeConfig,
    process::{Process, ProcessState, Stoppable},
};
use failure::Fallible;
use log::info;
use std::fs::{self, create_dir_all};

pub struct Scheduler {
    process: Process,
}

impl Scheduler {
    pub fn start(config: &Config, kubeconfig: &KubeConfig) -> ProcessState {
        info!("Starting Scheduler");

        let dir = config.root().join("scheduler");
        create_dir_all(&dir)?;

        let yml = format!(
            include_str!("assets/scheduler.yml"),
            kubeconfig.scheduler().display()
        );
        let cfg = &dir.join("config.yml");

        if !cfg.exists() {
            fs::write(cfg, yml)?;
        }

        let mut process = Process::start(
            &dir,
            "Scheduler",
            "kube-scheduler",
            &[&format!("--config={}", cfg.display()), "--v=2"],
        )?;

        process.wait_ready("Serving securely")?;
        info!("Scheduler is ready");
        Ok(Box::new(Self { process }))
    }
}

impl Stoppable for Scheduler {
    fn stop(&mut self) -> Fallible<()> {
        self.process.stop()
    }
}
