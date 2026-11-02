//! Kubernetes API server component.
//!
//! Starts `kube-apiserver` with TLS, etcd connectivity, RBAC authorization,
//! the NodeRestriction admission plugin and encryption-at-rest, then
//! applies the kubelet API RBAC rules. Anonymous requests are rejected.

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

        // In root mode, the API server advertises the node IP of the first
        // node instead of the address of the default route of the host, see
        // Network::advertise_address.
        let advertise = network
            .advertise_address()?
            .map(|ip| format!("--advertise-address={}", ip));
        let args: &[&str] = &[
            // Like kubeadm and kind, workloads like node agents need it.
            "--allow-privileged=true",
            // Nothing needs unauthenticated access, which would expose
            // for example the version to the network in root mode.
            "--anonymous-auth=false",
            "--audit-log-maxage=30",
            "--audit-log-maxbackup=3",
            "--audit-log-maxsize=100",
            &format!("--audit-log-path={}", dir.join("audit.log").display()),
            &format!("--audit-policy-file={}", audit_policy.display()),
            "--authorization-mode=Node,RBAC",
            "--bind-address=0.0.0.0",
            &format!("--client-ca-file={}", pki.ca().cert().display()),
            "--enable-admission-plugins=NodeRestriction",
            &format!("--etcd-cafile={}", pki.etcd_ca().cert().display()),
            &format!(
                "--etcd-certfile={}",
                pki.apiserver_etcd_client().cert().display()
            ),
            &format!(
                "--etcd-keyfile={}",
                pki.apiserver_etcd_client().key().display()
            ),
            &format!("--etcd-servers=https://{}", network.etcd_client()),
            "--event-ttl=1h",
            &format!(
                "--encryption-provider-config={}",
                encryptionconfig.path().display()
            ),
            &format!(
                "--kubelet-certificate-authority={}",
                pki.kubelet_ca().cert().display()
            ),
            &format!(
                "--kubelet-client-certificate={}",
                pki.apiserver_kubelet_client().cert().display()
            ),
            &format!(
                "--kubelet-client-key={}",
                pki.apiserver_kubelet_client().key().display()
            ),
            // The node names do not resolve if the hosts file cannot be
            // written (see System::setup), while the node IPs are in the
            // kubelet certificates and the kubelets listen on all
            // addresses, with a port per node.
            "--kubelet-preferred-address-types=InternalIP,Hostname",
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
                pki.service_account().public_key().display()
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
        ];
        let args = advertise
            .as_deref()
            .into_iter()
            .chain(args.iter().copied())
            .collect::<Vec<_>>();
        let mut process = Process::start(&dir, "API Server", "kube-apiserver", &args)?;

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
    use serde::Deserialize;
    use serde_norway::{Deserializer, Value};

    #[test]
    fn rbac_asset_contains_expected_rules() {
        let docs: Vec<Value> = Deserializer::from_str(include_str!("assets/apiserver.yml"))
            .map(|d| Value::deserialize(d).unwrap())
            .collect();
        assert_eq!(docs.len(), 2);

        let role = &docs[0];
        assert_eq!(role["kind"].as_str(), Some("ClusterRole"));
        let name = role["metadata"]["name"].as_str().unwrap();
        assert_eq!(name, "system:kube-apiserver-to-kubelet");
        let rule = &role["rules"][0];
        let strings = |v: &Value| -> Vec<String> {
            v.as_sequence()
                .unwrap()
                .iter()
                .map(|s| s.as_str().unwrap().to_owned())
                .collect()
        };
        assert_eq!(strings(&rule["apiGroups"]), [""]);
        assert_eq!(strings(&rule["verbs"]), ["*"]);
        let resources = strings(&rule["resources"]);
        for resource in ["nodes/proxy", "nodes/stats", "nodes/log", "nodes/metrics"] {
            assert!(resources.iter().any(|r| r == resource), "{}", resource);
        }

        // The user of the kubelet client certificate of the API server
        let binding = &docs[1];
        assert_eq!(binding["kind"].as_str(), Some("ClusterRoleBinding"));
        assert_eq!(binding["roleRef"]["kind"].as_str(), Some("ClusterRole"));
        assert_eq!(binding["roleRef"]["name"].as_str(), Some(name));
        assert_eq!(binding["subjects"][0]["kind"].as_str(), Some("User"));
        assert_eq!(
            binding["subjects"][0]["name"].as_str(),
            Some("kube-apiserver-kubelet-client")
        );
    }

    #[test]
    fn audit_policy_asset_logs_metadata() {
        let policy = include_str!("assets/audit-policy.yml");
        assert!(policy.contains("apiVersion: audit.k8s.io/v1"));
        assert!(policy.contains("kind: Policy"));
        // The catch-all rule has to be the last one
        assert!(policy.trim_end().ends_with("- level: Metadata"));
    }
}
