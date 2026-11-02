//! Extensible component registry for cluster bootstrapping.
//!
//! Each cluster component (etcd, API server, etc.) implements the [`Component`]
//! trait. Components declare their boot phase, which determines the order in
//! which they are started. Within a phase, components start in parallel.

use crate::{
    config::Config,
    encryptionconfig::EncryptionConfig,
    kubeconfig::KubeConfig,
    kubectl::Kubectl,
    network::Network,
    pki::Pki,
    process::{self, Canceled, ProcessState, Stoppables},
    signals,
};
use anyhow::anyhow;
use log::{debug, error, info};
use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
};

/// Boot phases control the startup order of cluster components.
/// Components in the same phase start concurrently; phases run sequentially.
///
/// Phase ordering is optimized so that independent components overlap:
/// the CRI runtime (CRI-O or containerd) and kube-proxy start alongside the
/// controllers, since they only need the API server, not the controllers.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Phase {
    /// Infrastructure services (etcd)
    Infrastructure,
    /// Control plane services (API server)
    ControlPlane,
    /// Controllers, node runtimes, and proxy all start together since they
    /// only depend on the API server, not on each other.
    /// Contains: scheduler, controller-manager, CRI runtime (per node), proxy.
    Controller,
    /// Node agents that require a running CRI runtime (kubelet).
    NodeAgent,
}

/// Shared cluster state passed to components during startup.
pub struct ClusterContext<'a> {
    /// Cluster configuration
    pub config: &'a Config,
    /// Network topology
    pub network: &'a Network,
    /// Public key infrastructure (certificates)
    pub pki: &'a Pki,
    /// Kubeconfig files for each component
    pub kubeconfig: &'a KubeConfig,
    /// Encryption configuration for the API server
    pub encryptionconfig: &'a EncryptionConfig,
    /// Kubectl handle for RBAC and workload operations
    pub kubectl: &'a Kubectl,
}

/// Trait implemented by each cluster component.
///
/// This provides a uniform interface for the orchestrator to start the
/// registered components and to order them by their boot phase.
pub trait Component: Send + Sync {
    /// Human-readable name for logging
    fn name(&self) -> &str;

    /// The boot phase this component belongs to
    fn phase(&self) -> Phase;

    /// Start the component. Returns a handle that can stop it.
    fn start(&self, ctx: &ClusterContext<'_>) -> ProcessState;
}

/// A registry that collects components and groups them by phase.
#[must_use]
pub struct ComponentRegistry {
    components: Vec<Box<dyn Component>>,
}

impl ComponentRegistry {
    /// Create an empty registry
    pub fn new() -> Self {
        Self {
            components: Vec::new(),
        }
    }

    /// Register a component
    pub fn register(&mut self, component: Box<dyn Component>) {
        self.components.push(component);
    }

    /// Return components grouped and sorted by phase
    pub fn by_phase(&self) -> Vec<(Phase, Vec<&dyn Component>)> {
        let mut phases: Vec<Phase> = self.components.iter().map(|c| c.phase()).collect();
        phases.sort();
        phases.dedup();

        phases
            .into_iter()
            .map(|phase| {
                let group: Vec<&dyn Component> = self
                    .components
                    .iter()
                    .filter(|c| c.phase() == phase)
                    .map(|c| c.as_ref())
                    .collect();
                (phase, group)
            })
            .collect()
    }

    /// Execute all registered components phase by phase.
    ///
    /// Within each phase, components start in parallel, each on its own
    /// thread since a start blocks until the component is ready. Once a
    /// component fails, the others of the phase stop waiting for their
    /// readiness.
    /// If any component in a phase fails or a termination signal has been
    /// received, subsequent phases are skipped (later phases depend on
    /// earlier ones).
    ///
    /// The Controller phase runs the controllers, the CRI runtimes and the
    /// proxy in parallel, since they all only depend on the API server.
    ///
    /// Returns stoppable handles grouped by phase in reverse phase order
    /// (for proper shutdown: node agents before runtimes, runtimes before
    /// control plane, etc.) and the startup failures.
    pub fn run(&self, ctx: &ClusterContext<'_>) -> (Vec<Stoppables>, Vec<String>) {
        info!("Starting processes");
        let mut groups: Vec<Stoppables> = Vec::new();
        let mut failures: Vec<String> = Vec::new();

        for (phase, components) in &self.by_phase() {
            if let Err(e) = signals::check() {
                failures.push(e.to_string());
                break;
            }
            debug!(
                "Starting {:?} phase ({} components)",
                phase,
                components.len()
            );
            let (group, errors) = Self::start_phase(components, |c| c.start(ctx));
            groups.push(group);
            failures.extend(errors);

            if !failures.is_empty() {
                error!(
                    "Phase {:?} failed ({} errors: {}), skipping remaining phases",
                    phase,
                    failures.len(),
                    failures.join(", ")
                );
                break;
            }
        }

        groups.reverse();
        (groups, failures)
    }

    /// Start the components of a phase in parallel via `start`. Returns the
    /// started ones and the failures, without the cancellations which a
    /// failure caused.
    fn start_phase<F>(components: &[&dyn Component], start: F) -> (Stoppables, Vec<String>)
    where
        F: Fn(&dyn Component) -> ProcessState + Sync,
    {
        let cancel = Arc::new(AtomicBool::new(false));
        let results: Vec<ProcessState> = thread::scope(|s| {
            let handles: Vec<_> = components
                .iter()
                .map(|c| {
                    let cancel = Arc::clone(&cancel);
                    let start = &start;
                    s.spawn(move || {
                        debug!("Starting {}", c.name());
                        process::set_cancel(Arc::clone(&cancel));
                        let result = start(*c);
                        if result.is_err() {
                            cancel.store(true, Ordering::SeqCst);
                        }
                        result
                    })
                })
                .collect();
            handles
                .into_iter()
                .zip(components)
                .map(|(h, c)| {
                    h.join()
                        .unwrap_or_else(|_| Err(anyhow!("Starting {} panicked", c.name())))
                })
                .collect()
        });

        let mut group = Stoppables::new();
        let mut failures = Vec::new();
        for result in results {
            match result {
                Ok(p) => group.push(p),
                Err(e) if e.downcast_ref::<Canceled>().is_some() => debug!("{:#}", e),
                Err(e) => failures.push(format!("{:#}", e)),
            }
        }
        (group, failures)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::process::{Stoppable, canceled};
    use anyhow::{Context, Result, bail};
    use std::time::{Duration, Instant};

    struct FakeComponent {
        name: String,
        phase: Phase,
    }

    impl Component for FakeComponent {
        fn name(&self) -> &str {
            &self.name
        }

        fn phase(&self) -> Phase {
            self.phase
        }

        fn start(&self, _ctx: &ClusterContext<'_>) -> ProcessState {
            bail!("fake component")
        }
    }

    #[test]
    fn registry_groups_by_phase() {
        let mut reg = ComponentRegistry::new();
        reg.register(Box::new(FakeComponent {
            name: "etcd".into(),
            phase: Phase::Infrastructure,
        }));
        reg.register(Box::new(FakeComponent {
            name: "apiserver".into(),
            phase: Phase::ControlPlane,
        }));
        reg.register(Box::new(FakeComponent {
            name: "scheduler".into(),
            phase: Phase::Controller,
        }));
        reg.register(Box::new(FakeComponent {
            name: "controller-manager".into(),
            phase: Phase::Controller,
        }));

        let phases = reg.by_phase();
        assert_eq!(phases.len(), 3);
        assert_eq!(phases[0].0, Phase::Infrastructure);
        assert_eq!(phases[0].1.len(), 1);
        assert_eq!(phases[1].0, Phase::ControlPlane);
        assert_eq!(phases[1].1.len(), 1);
        assert_eq!(phases[2].0, Phase::Controller);
        assert_eq!(phases[2].1.len(), 2);
    }

    struct FakeStarted;

    impl Stoppable for FakeStarted {
        fn stop(&mut self) -> Result<()> {
            Ok(())
        }

        fn died(&mut self) -> bool {
            false
        }
    }

    /// Start the fake components: "fail" fails, "wait" waits until it gets
    /// canceled and the others succeed.
    fn start_fake(c: &dyn Component) -> ProcessState {
        match c.name() {
            "fail" => bail!("failed"),
            "wait" => {
                let deadline = Instant::now() + Duration::from_secs(10);
                while !canceled() {
                    if Instant::now() >= deadline {
                        bail!("not canceled")
                    }
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(Canceled).context("stopped waiting")
            }
            _ => Ok(Box::new(FakeStarted)),
        }
    }

    fn fake(name: &str) -> FakeComponent {
        FakeComponent {
            name: name.into(),
            phase: Phase::Controller,
        }
    }

    #[test]
    fn start_phase_cancels_siblings() {
        let (a, b, c) = (fake("ok"), fake("wait"), fake("fail"));
        let (group, failures) = ComponentRegistry::start_phase(&[&a, &b, &c], start_fake);
        assert_eq!(group.len(), 1);
        assert_eq!(failures, ["failed"]);

        let (group, failures) = ComponentRegistry::start_phase(&[&a, &a], start_fake);
        assert_eq!(group.len(), 2);
        assert!(failures.is_empty());
    }

    #[test]
    fn empty_registry() {
        let reg = ComponentRegistry::new();
        assert!(reg.by_phase().is_empty());
    }

    #[test]
    fn phase_ordering() {
        assert!(Phase::Infrastructure < Phase::ControlPlane);
        assert!(Phase::ControlPlane < Phase::Controller);
        assert!(Phase::Controller < Phase::NodeAgent);
    }
}
