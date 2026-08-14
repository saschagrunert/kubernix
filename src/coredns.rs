//! CoreDNS addon deployment for the cluster.

use crate::{
    API_SERVER_PORT, config::Config, kubectl::Kubectl, network::Network, write_if_changed,
};
use anyhow::{Context, Result};
use log::info;
use std::{fs::create_dir_all, net::Ipv4Addr};

/// Deploys the CoreDNS addon to the cluster.
pub struct CoreDns;

impl CoreDns {
    /// Render the CoreDNS manifest, apply it, and wait for the pod to be ready.
    pub fn apply(config: &Config, network: &Network, kubectl: &Kubectl) -> Result<()> {
        info!("Deploying CoreDNS and waiting to be ready");

        let dir = config.root().join("coredns");
        create_dir_all(&dir)?;

        let yml = Self::render(network.dns()?, config.is_rootless());
        let file = dir.join("coredns.yml");
        write_if_changed(&file, &yml)?;

        kubectl.apply(&file).context("Unable to deploy CoreDNS")?;
        kubectl.wait_ready("coredns")?;
        info!("CoreDNS deployed");
        Ok(())
    }

    fn render(dns: Ipv4Addr, skip_resources: bool) -> String {
        // CoreDNS uses hostNetwork, so it can always reach the API server
        // at 127.0.0.1. Setting these env vars explicitly avoids depending
        // on kube-proxy ClusterIP routing being ready when CoreDNS starts.
        let env = format!(
            concat!(
                "        env:\n",
                "        - name: KUBERNETES_SERVICE_HOST\n",
                "          value: \"127.0.0.1\"\n",
                "        - name: KUBERNETES_SERVICE_PORT\n",
                "          value: \"{}\"\n",
            ),
            API_SERVER_PORT,
        );
        let resources = if skip_resources {
            ""
        } else {
            concat!(
                "        resources:\n",
                "          limits:\n",
                "            memory: 170Mi\n",
                "          requests:\n",
                "            cpu: 100m\n",
                "            memory: 70Mi\n",
            )
        };
        format!(
            include_str!("assets/coredns.yml"),
            dns,
            env = env,
            resources = resources,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_contains_dns_ip() {
        let ip = Ipv4Addr::new(10, 10, 1, 2);
        let yml = CoreDns::render(ip, false);
        assert!(yml.contains("clusterIP: 10.10.1.2"));
        assert!(yml.contains("k8s-app: coredns"));
    }

    #[test]
    fn render_contains_resource_kinds() {
        let yml = CoreDns::render(Ipv4Addr::new(10, 0, 0, 2), false);
        assert!(yml.contains("kind: ServiceAccount"));
        assert!(yml.contains("kind: ClusterRole"));
        assert!(yml.contains("kind: ClusterRoleBinding"));
        assert!(yml.contains("kind: ConfigMap"));
        assert!(yml.contains("kind: Deployment"));
        assert!(yml.contains("kind: Service"));
    }

    #[test]
    fn render_always_sets_api_server_env() {
        for rootless in [true, false] {
            let yml = CoreDns::render(Ipv4Addr::new(10, 10, 1, 2), rootless);
            assert!(yml.contains("KUBERNETES_SERVICE_HOST"));
            assert!(yml.contains("value: \"127.0.0.1\""));
            assert!(yml.contains("KUBERNETES_SERVICE_PORT"));
            assert!(yml.contains(&format!("value: \"{}\"", crate::API_SERVER_PORT)));
        }
    }

    #[test]
    fn render_non_rootless_has_resources() {
        let yml = CoreDns::render(Ipv4Addr::new(10, 10, 1, 2), false);
        assert!(yml.contains("memory: 170Mi"));
        assert!(yml.contains("cpu: 100m"));
    }

    #[test]
    fn render_rootless_no_resources() {
        let yml = CoreDns::render(Ipv4Addr::new(10, 10, 1, 2), true);
        assert!(!yml.contains("memory: 170Mi"));
    }

    #[test]
    fn render_produces_valid_structure() {
        for rootless in [true, false] {
            let yml = CoreDns::render(Ipv4Addr::new(10, 10, 1, 2), rootless);
            assert!(yml.contains("KUBERNETES_SERVICE_HOST"));
            let args_pos = yml.find("        args:").unwrap();
            let env_pos = yml.find("        env:\n").unwrap();
            let mounts_pos = yml.find("        volumeMounts:").unwrap();
            assert!(args_pos < env_pos, "args must precede env");
            assert!(env_pos < mounts_pos, "env must precede volumeMounts");
        }
    }
}
