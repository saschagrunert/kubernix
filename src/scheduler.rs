use crate::{
    config::Config,
    kubeconfig::KubeConfig,
    process::{Process, Startable, Stoppable},
};
use failure::Fallible;
use log::info;
use std::fs::{self, create_dir_all};

pub struct Scheduler {
    process: Process,
}

impl Scheduler {
    pub fn start(config: &Config, kubeconfig: &KubeConfig) -> Fallible<Startable> {
        info!("Starting Scheduler");

        let dir = config.root().join("scheduler");
        create_dir_all(&dir)?;

        let yml = format!(
            include_str!("assets/scheduler.yml"),
            kubeconfig.scheduler.display()
        );
        let cfg = &dir.join("config.yml");
        fs::write(cfg, yml)?;

        let mut process = Process::start(
            config,
            &[
                "kube-scheduler".to_owned(),
                format!("--config={}", cfg.display()),
                "--v=2".to_owned(),
            ],
        )?;

        process.wait_ready("Serving securely")?;
        info!("Scheduler is ready");
        Ok(Box::new(Scheduler { process }))
    }
}

impl Stoppable for Scheduler {
    fn stop(&mut self) -> Fallible<()> {
        self.process.stop()
    }
}
