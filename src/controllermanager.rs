//! Kubernetes controller manager component.
//!
//! Runs `kube-controller-manager` which watches cluster state via the
//! API server and drives the actual state toward the desired state.

use crate::{
    component::{ClusterContext, Component, Phase},
    config::Config,
    kubeconfig::KubeConfig,
    network::Network,
    pki::Pki,
    process::{Process, ProcessState, stoppable},
};
use anyhow::Context;
use std::fs::create_dir_all;

/// Component wrapper for registry-based startup.
pub struct ControllerManagerComponent;

impl Component for ControllerManagerComponent {
    fn name(&self) -> &str {
        "Controller Manager"
    }

    fn phase(&self) -> Phase {
        Phase::Controller
    }

    fn start(&self, ctx: &ClusterContext<'_>) -> ProcessState {
        ControllerManager::start(ctx.config, ctx.network, ctx.pki, ctx.kubeconfig)
    }
}

/// Manages the `kube-controller-manager` process lifecycle.
pub struct ControllerManager {
    process: Process,
}

impl ControllerManager {
    /// Start the controller manager with the given cluster configuration.
    pub fn start(
        config: &Config,
        network: &Network,
        pki: &Pki,
        kubeconfig: &KubeConfig,
    ) -> ProcessState {
        let dir = config.root().join("controllermanager");
        create_dir_all(&dir).context("Unable to create controller-manager directory")?;

        let mut args = vec![
            "--bind-address=127.0.0.1".into(),
            format!("--cluster-cidr={}", network.cluster_cidr()),
            "--cluster-name=kubernetes".into(),
        ];
        args.extend(Self::signing_args(pki));
        args.extend([
            format!("--kubeconfig={}", kubeconfig.controller_manager().display()),
            "--leader-elect=false".into(),
            format!(
                "--requestheader-client-ca-file={}",
                pki.front_proxy_ca().cert().display()
            ),
            format!("--root-ca-file={}", pki.ca().cert().display()),
            format!(
                "--service-account-private-key-file={}",
                pki.service_account().key().display()
            ),
            format!("--service-cluster-ip-range={}", network.service_cidr()),
            "--use-service-account-credentials=true".into(),
            "--v=2".into(),
        ]);
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        let mut process =
            Process::start(&dir, "Controller Manager", "kube-controller-manager", &args)?;

        process.wait_ready("Serving securely")?;
        Ok(Box::new(Self { process }))
    }

    /// The CAs which sign the approved certificate signing requests per
    /// signer. The kubelet serving certificates have to be signed by the
    /// kubelet serving CA, since the API server only trusts it for the
    /// connections to the kubelets. The cluster CA signs all client
    /// certificates, which the API server trusts.
    fn signing_args(pki: &Pki) -> Vec<String> {
        let ca = pki.ca();
        [
            ("kube-apiserver-client", ca),
            ("kubelet-client", ca),
            ("kubelet-serving", pki.kubelet_ca()),
            ("legacy-unknown", ca),
        ]
        .iter()
        .flat_map(|(signer, ca)| {
            [
                format!(
                    "--cluster-signing-{}-cert-file={}",
                    signer,
                    ca.cert().display()
                ),
                format!(
                    "--cluster-signing-{}-key-file={}",
                    signer,
                    ca.key().display()
                ),
            ]
        })
        .collect()
    }
}

stoppable!(ControllerManager);

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{config::tests::test_config, network::tests::test_network};
    use anyhow::Result;

    #[test]
    fn signing_args_use_kubelet_serving_ca() -> Result<()> {
        let c = test_config()?;
        let n = test_network()?;
        let pki = Pki::new(&c, &n)?;
        let args = ControllerManager::signing_args(&pki);
        assert_eq!(args.len(), 8);
        let ca = pki.ca().cert().display().to_string();
        let kubelet_ca = pki.kubelet_ca().key().display().to_string();
        assert!(args.contains(&format!(
            "--cluster-signing-kubelet-client-cert-file={}",
            ca
        )));
        assert!(args.contains(&format!(
            "--cluster-signing-kubelet-serving-key-file={}",
            kubelet_ca
        )));
        assert_eq!(
            args.iter()
                .filter(|a| a.ends_with(&format!("={}", ca)))
                .count(),
            3
        );
        // Generic flags must not be combined with the specific ones
        assert!(
            !args
                .iter()
                .any(|a| a.starts_with("--cluster-signing-cert-file"))
        );
        Ok(())
    }
}
