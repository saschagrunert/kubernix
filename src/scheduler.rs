use crate::{
    component::{ClusterContext, Component, Phase},
    config::Config,
    kubeconfig::KubeConfig,
    process::{Process, ProcessState, Stoppable},
};
use anyhow::Result;
use std::fs::{self, create_dir_all};

/// Component wrapper for registry-based startup.
pub struct SchedulerComponent;

impl Component for SchedulerComponent {
    fn name(&self) -> &str {
        "Scheduler"
    }

    fn phase(&self) -> Phase {
        Phase::Controller
    }

    fn start(&self, ctx: &ClusterContext<'_>) -> ProcessState {
        Scheduler::start(ctx.config, ctx.kubeconfig)
    }
}

pub struct Scheduler {
    process: Process,
}

impl Scheduler {
    pub fn start(config: &Config, kubeconfig: &KubeConfig) -> ProcessState {
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
        Ok(Box::new(Self { process }))
    }
}

impl Stoppable for Scheduler {
    fn stop(&mut self) -> Result<()> {
        self.process.stop()
    }
}
