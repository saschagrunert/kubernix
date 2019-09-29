use crate::{
    config::Config,
    kubeconfig::KubeConfig,
    process::{Process, Stoppable},
};
use failure::Fallible;
use incdoc::incdoc;
use log::info;
use std::fs::{self, create_dir_all};

pub struct Scheduler {
    process: Process,
}

impl Scheduler {
    pub fn new(
        config: &Config,
        kubeconfig: &KubeConfig,
    ) -> Fallible<Scheduler> {
        info!("Starting Scheduler");

        let dir = config.root.join("scheduler");
        create_dir_all(&dir)?;

        let yml = incdoc!(format!(
            r#"
               ---
               apiVersion: kubescheduler.config.k8s.io/v1alpha1
               kind: KubeSchedulerConfiguration
               clientConnection:
                 kubeconfig: "{}"
               leaderElection:
                 leaderElect: true
               "#,
            kubeconfig.scheduler.display()
        ));
        let cfg = &dir.join("config.yml");
        fs::write(cfg, yml)?;

        let mut process = Process::new(
            config,
            &[
                "kube-scheduler".to_owned(),
                format!("--config={}", cfg.display()),
                "--v=2".to_owned(),
            ],
        )?;

        process.wait_ready("Serving securely")?;
        info!("Scheduler is ready");
        Ok(Scheduler { process })
    }
}

impl Stoppable for Scheduler {
    fn stop(&mut self) {
        self.process.stop();
    }
}
