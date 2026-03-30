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
    process::{ProcessState, Started, Stoppables},
};
use log::{debug, info};
use rayon::prelude::*;

/// Boot phases control the startup order of cluster components.
/// Components in the same phase start concurrently; phases run sequentially.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Phase {
    /// Infrastructure services (etcd)
    Infrastructure,
    /// Control plane services (API server)
    ControlPlane,
    /// Controllers that depend on the API server (scheduler, controller-manager)
    Controller,
    /// Node-level container runtimes (CRI-O); must be ready before NodeAgent
    NodeRuntime,
    /// Node agents that require a running runtime (kubelet, proxy)
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
/// This provides a uniform interface for the orchestrator to discover,
/// start, and order components without hardcoding them in `lib.rs`.
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
    /// Within each phase, components start in parallel via rayon.
    /// If any component in a phase fails, subsequent phases are skipped
    /// (later phases depend on earlier ones).
    ///
    /// Note: components within the same phase start concurrently, so
    /// CRI-O (NodeRuntime) does not overlap with the control plane.
    /// This trades some startup speed for simpler, predictable ordering.
    ///
    /// Returns stoppable handles in reverse phase order (for proper
    /// shutdown: node agents before runtimes, runtimes before control
    /// plane, etc.) and a flag indicating whether all succeeded.
    pub fn run(&self, ctx: &ClusterContext<'_>) -> (Stoppables, bool) {
        info!("Starting processes");
        let mut all_results: Vec<(Phase, ProcessState)> = Vec::new();

        for (phase, components) in &self.by_phase() {
            debug!(
                "Starting {:?} phase ({} components)",
                phase,
                components.len()
            );
            let results: Vec<ProcessState> = components
                .par_iter()
                .map(|c| {
                    debug!("Starting {}", c.name());
                    c.start(ctx)
                })
                .collect();

            let phase_ok = results.iter().all(|r| r.is_ok());
            all_results.extend(results.into_iter().map(|r| (*phase, r)));

            if !phase_ok {
                info!("Phase {:?} failed, skipping remaining phases", phase);
                break;
            }
        }

        let all_ok = all_results.iter().all(|(_, r)| r.is_ok());

        // Collect successful starts, then reverse so shutdown happens
        // in reverse phase order.
        let mut stoppables: Vec<(Phase, Started)> = all_results
            .into_iter()
            .filter_map(|(phase, r)| match r {
                Ok(p) => Some((phase, p)),
                Err(e) => {
                    debug!("{}", e);
                    None
                }
            })
            .collect();
        stoppables.reverse();

        let processes: Stoppables = stoppables.into_iter().map(|(_, p)| p).collect();
        (processes, all_ok)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::bail;

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

    #[test]
    fn empty_registry() {
        let reg = ComponentRegistry::new();
        assert!(reg.by_phase().is_empty());
    }
}
