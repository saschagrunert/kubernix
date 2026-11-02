//! Kubernetes API server component.
//!
//! Starts `kube-apiserver` with TLS, etcd connectivity, RBAC authorization,
//! and encryption-at-rest, then applies the kubelet API RBAC rules.

use crate::{
    component::{ClusterContext, Component, Phase},
    config::Config,
    encryptionconfig::EncryptionConfig,
    kubectl::Kubectl,
    network::Network,
    pki::Pki,
    process::{Process, ProcessState, stoppable},
    write_if_changed,
};
use anyhow::{Context, Result};
use log::debug;
use std::{fs::create_dir_all, path::Path};

/// Component wrapper for registry-based startup.
pub struct ApiServerComponent;

impl Component for ApiServerComponent {
    fn name(&self) -> &str {
        "API Server"
    }

    fn phase(&self) -> Phase {
        Phase::ControlPlane
    }

    fn start(&self, ctx: &ClusterContext<'_>) -> ProcessState {
        ApiServer::start(
            ctx.config,
            ctx.network,
            ctx.pki,
            ctx.encryptionconfig,
            ctx.kubectl,
        )
    }
}

/// Manages the `kube-apiserver` process lifecycle.
pub struct ApiServer {
    process: Process,
}

impl ApiServer {
    /// Start the API server with the given cluster configuration.
    pub fn start(
        config: &Config,
        network: &Network,
        pki: &Pki,
        encryptionconfig: &EncryptionConfig,
        kubectl: &Kubectl,
    ) -> ProcessState {
        let dir = config.root().join("apiserver");
        create_dir_all(&dir).context("Unable to create API server directory")?;

        let audit_policy = dir.join("audit-policy.yml");
        write_if_changed(&audit_policy, include_str!("assets/audit-policy.yml"))?;

        let mut process = Process::start(
            &dir,
            "API Server",
            "kube-apiserver",
            &[
                // Like kubeadm and kind, workloads like node agents need it.
                "--allow-privileged=true",
                "--audit-log-maxage=30",
                "--audit-log-maxbackup=3",
                "--audit-log-maxsize=100",
                &format!("--audit-log-path={}", dir.join("audit.log").display()),
                &format!("--audit-policy-file={}", audit_policy.display()),
                "--authorization-mode=Node,RBAC",
                "--bind-address=0.0.0.0",
                &format!("--client-ca-file={}", pki.ca().cert().display()),
                &format!("--etcd-cafile={}", pki.ca().cert().display()),
                &format!("--etcd-certfile={}", pki.apiserver().cert().display()),
                &format!("--etcd-keyfile={}", pki.apiserver().key().display()),
                &format!("--etcd-servers=https://{}", network.etcd_client()),
                "--event-ttl=1h",
                &format!(
                    "--encryption-provider-config={}",
                    encryptionconfig.path().display()
                ),
                &format!(
                    "--kubelet-certificate-authority={}",
                    pki.ca().cert().display()
                ),
                &format!(
                    "--kubelet-client-certificate={}",
                    pki.apiserver().cert().display()
                ),
                &format!("--kubelet-client-key={}", pki.apiserver().key().display()),
                // Request header authentication for aggregated API servers
                &format!(
                    "--proxy-client-cert-file={}",
                    pki.front_proxy_client().cert().display()
                ),
                &format!(
                    "--proxy-client-key-file={}",
                    pki.front_proxy_client().key().display()
                ),
                &format!(
                    "--requestheader-allowed-names={}",
                    pki.front_proxy_client().user()
                ),
                &format!(
                    "--requestheader-client-ca-file={}",
                    pki.front_proxy_ca().cert().display()
                ),
                "--requestheader-extra-headers-prefix=X-Remote-Extra-",
                "--requestheader-group-headers=X-Remote-Group",
                "--requestheader-username-headers=X-Remote-User",
                "--service-account-issuer=https://kubernetes.default.svc.cluster.local",
                &format!(
                    "--service-account-key-file={}",
                    pki.service_account().cert().display()
                ),
                &format!(
                    "--service-account-signing-key-file={}",
                    pki.service_account().key().display()
                ),
                &format!("--service-cluster-ip-range={}", network.service_cidr()),
                &format!("--tls-cert-file={}", pki.apiserver().cert().display()),
                &format!("--tls-private-key-file={}", pki.apiserver().key().display()),
                "--profiling=false",
                "--v=2",
            ],
        )?;

        process.wait_ready("Serving securely")?;
        // The log line only indicates that the server listens, controllers
        // started right after it may fail on post start hooks which are not
        // done yet (for example rbac/bootstrap-roles).
        kubectl.wait_api_ready()?;
        Self::setup_rbac(&dir, kubectl)?;
        Ok(Box::new(Self { process }))
    }

    fn setup_rbac(dir: &Path, kubectl: &Kubectl) -> Result<()> {
        debug!("Creating API Server RBAC rule for kubelet");
        let file = dir.join("rbac.yml");
        write_if_changed(&file, include_str!("assets/apiserver.yml"))?;

        kubectl
            .apply(&file)
            .context("Unable to deploy API server RBAC rules")?;

        debug!("API Server RBAC rule created");
        Ok(())
    }
}

stoppable!(ApiServer);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rbac_asset_contains_expected_rules() {
        let rbac = include_str!("assets/apiserver.yml");
        assert!(rbac.contains("kind: ClusterRole"));
        assert!(rbac.contains("kind: ClusterRoleBinding"));
        assert!(rbac.contains("system:kube-apiserver-to-kubelet"));
        assert!(rbac.contains("nodes/proxy"));
        assert!(rbac.contains("nodes/stats"));
    }

    #[test]
    fn audit_policy_asset_logs_metadata() {
        let policy = include_str!("assets/audit-policy.yml");
        assert!(policy.contains("apiVersion: audit.k8s.io/v1"));
        assert!(policy.contains("kind: Policy"));
        // The catch-all rule has to be the last one
        assert!(policy.trim_end().ends_with("- level: Metadata"));
    }

    #[test]
    fn component_metadata() {
        let c = ApiServerComponent;
        assert_eq!(c.name(), "API Server");
        assert_eq!(c.phase(), Phase::ControlPlane);
    }
}
