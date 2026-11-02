//! CoreDNS addon deployment for the cluster.

use crate::{
    API_SERVER_PORT, config::Config, kubectl::Kubectl, network::Network, write_if_changed,
};
use anyhow::{Context, Result};
use log::debug;
use std::{fs::create_dir_all, net::Ipv4Addr, sync::atomic::AtomicBool};

// CoreDNS runs in the host network, so its DNS server listens on an
// unprivileged port, which the Service maps to port 53, and its health and
// readiness ports replace the defaults 8080 and 8181. So they do not
// collide with other services on the host, like a DNS server listening on
// port 53 of all addresses.
const DNS_PORT: u16 = 10053;
const HEALTH_PORT: u16 = 10054;
const READY_PORT: u16 = 10055;
const METRICS_PORT: u16 = 9153;

/// Deploys the CoreDNS addon to the cluster.
pub struct CoreDns;

impl CoreDns {
    /// Render the CoreDNS manifest, apply it, and wait for the pod to be ready
    /// or until `shutdown` is set.
    pub fn apply(
        config: &Config,
        network: &Network,
        kubectl: &Kubectl,
        shutdown: &AtomicBool,
    ) -> Result<()> {
        debug!("Deploying CoreDNS and waiting to be ready");

        let dir = config.root().join("coredns");
        create_dir_all(&dir).context("Unable to create CoreDNS directory")?;

        let yml = Self::render(network.dns()?, config.is_rootless());
        let file = dir.join("coredns.yml");
        write_if_changed(&file, &yml)?;

        kubectl.apply(&file).context("Unable to deploy CoreDNS")?;
        kubectl.wait_ready("coredns", shutdown)?;
        debug!("CoreDNS deployed");
        Ok(())
    }

    fn render(dns: Ipv4Addr, skip_resources: bool) -> String {
        // CoreDNS uses hostNetwork, so it can always reach the API server
        // at 127.0.0.1. Setting these env vars explicitly avoids depending
        // on kube-proxy ClusterIP routing being ready when CoreDNS starts.
        // All servers bind only the pod IP, which is the node IP with
        // hostNetwork and the endpoint of the Service. It is the gateway
        // address of the bridge of the node in root mode (see
        // Network::node_ip), so that the resolver and the metrics do not
        // listen on the addresses of the host, which other machines use.
        // The address is not advertised, but a machine in the same layer 2
        // network which routes the CIDR via the host can still reach it,
        // since the host accepts packets for all of its addresses on every
        // interface. In rootless mode, it is the address of the network
        // namespace of rootlesskit.
        let env = format!(
            concat!(
                "        env:\n",
                "        - name: KUBERNETES_SERVICE_HOST\n",
                "          value: \"127.0.0.1\"\n",
                "        - name: KUBERNETES_SERVICE_PORT\n",
                "          value: \"{}\"\n",
                "        - name: POD_IP\n",
                "          valueFrom:\n",
                "            fieldRef:\n",
                "              fieldPath: status.podIP\n",
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
            dns = dns,
            env = env,
            dns_port = DNS_PORT,
            health_port = HEALTH_PORT,
            ready_port = READY_PORT,
            metrics_port = METRICS_PORT,
            resources = resources,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;
    use serde_norway::{Deserializer, Value};

    const DNS: Ipv4Addr = Ipv4Addr::new(10, 10, 1, 2);

    /// Render and parse the manifest into its documents.
    fn documents(rootless: bool) -> Vec<Value> {
        Deserializer::from_str(&CoreDns::render(DNS, rootless))
            .map(|d| Value::deserialize(d).unwrap())
            .collect()
    }

    fn kind<'a>(docs: &'a [Value], kind: &str) -> &'a Value {
        docs.iter()
            .find(|d| d["kind"].as_str() == Some(kind))
            .unwrap_or_else(|| panic!("no {} in the manifest", kind))
    }

    fn container(docs: &[Value]) -> &Value {
        let containers = &kind(docs, "Deployment")["spec"]["template"]["spec"]["containers"];
        assert_eq!(containers.as_sequence().unwrap().len(), 1);
        &containers[0]
    }

    fn env<'a>(docs: &'a [Value], name: &str) -> &'a Value {
        container(docs)["env"]
            .as_sequence()
            .unwrap()
            .iter()
            .find(|e| e["name"].as_str() == Some(name))
            .unwrap_or_else(|| panic!("no env {}", name))
    }

    fn corefile(docs: &[Value]) -> &str {
        kind(docs, "ConfigMap")["data"]["Corefile"]
            .as_str()
            .unwrap()
    }

    #[test]
    fn render_contains_resource_kinds() {
        for rootless in [true, false] {
            let kinds: Vec<_> = documents(rootless)
                .iter()
                .map(|d| d["kind"].as_str().unwrap().to_owned())
                .collect();
            assert_eq!(
                kinds,
                [
                    "ServiceAccount",
                    "ClusterRole",
                    "ClusterRoleBinding",
                    "ConfigMap",
                    "Deployment",
                    "Service",
                ]
            );
        }
    }

    #[test]
    fn render_service_uses_dns_ip() {
        let docs = documents(false);
        let service = &kind(&docs, "Service")["spec"];
        assert_eq!(service["clusterIP"].as_str(), Some("10.10.1.2"));
        assert_eq!(service["selector"]["k8s-app"].as_str(), Some("coredns"));
        let deployment = &kind(&docs, "Deployment")["spec"];
        assert_eq!(
            deployment["selector"]["matchLabels"]["k8s-app"].as_str(),
            Some("coredns")
        );
        assert_eq!(
            deployment["template"]["metadata"]["labels"]["k8s-app"].as_str(),
            Some("coredns")
        );
    }

    #[test]
    fn render_always_sets_api_server_env() {
        for rootless in [true, false] {
            let docs = documents(rootless);
            assert_eq!(
                env(&docs, "KUBERNETES_SERVICE_HOST")["value"].as_str(),
                Some("127.0.0.1")
            );
            assert_eq!(
                env(&docs, "KUBERNETES_SERVICE_PORT")["value"].as_str(),
                Some(API_SERVER_PORT.to_string().as_str())
            );
        }
    }

    #[test]
    fn render_binds_pod_ip() {
        for rootless in [true, false] {
            let docs = documents(rootless);
            let corefile = corefile(&docs);
            assert!(corefile.starts_with(".:10053 {\n    bind {$POD_IP}\n"));
            assert!(corefile.contains("    health {$POD_IP}:10054 {\n"));
            assert!(corefile.contains("    ready {$POD_IP}:10055\n"));
            assert!(corefile.contains("    prometheus {$POD_IP}:9153\n"));
            assert_eq!(
                env(&docs, "POD_IP")["valueFrom"]["fieldRef"]["fieldPath"].as_str(),
                Some("status.podIP")
            );
        }
    }

    /// The container ports by name.
    fn ports(docs: &[Value]) -> Vec<(String, u64)> {
        container(docs)["ports"]
            .as_sequence()
            .unwrap()
            .iter()
            .map(|p| {
                (
                    p["name"].as_str().unwrap().to_owned(),
                    p["containerPort"].as_u64().unwrap(),
                )
            })
            .collect()
    }

    #[test]
    fn render_uses_uncommon_ports() {
        let docs = documents(false);
        let port = |name: &str| {
            ports(&docs)
                .into_iter()
                .find(|(n, _)| n == name)
                .map(|(_, p)| p)
        };
        assert_eq!(port("dns"), Some(u64::from(DNS_PORT)));
        assert_eq!(port("dns-tcp"), Some(u64::from(DNS_PORT)));
        assert_eq!(port("metrics"), Some(u64::from(METRICS_PORT)));
        assert_eq!(port("liveness-probe"), Some(u64::from(HEALTH_PORT)));
        assert_eq!(port("readiness-probe"), Some(u64::from(READY_PORT)));
        assert!(ports(&docs).iter().all(|(_, p)| *p >= 1024));

        let yml = CoreDns::render(DNS, false);
        assert!(!yml.contains("8080"));
        assert!(!yml.contains("8181"));
        // The image grants the binary NET_BIND_SERVICE as file capability,
        // which has to stay in the bounding set for exec to succeed
        let capabilities = &container(&docs)["securityContext"]["capabilities"];
        assert_eq!(capabilities["add"][0].as_str(), Some("NET_BIND_SERVICE"));
        assert_eq!(capabilities["drop"][0].as_str(), Some("ALL"));
    }

    #[test]
    fn render_service_maps_dns_port() {
        let docs = documents(false);
        let ports: Vec<_> = kind(&docs, "Service")["spec"]["ports"]
            .as_sequence()
            .unwrap()
            .iter()
            .map(|p| {
                (
                    p["name"].as_str().unwrap(),
                    p["port"].as_u64().unwrap(),
                    p["targetPort"].as_str().unwrap(),
                    p["protocol"].as_str().unwrap(),
                )
            })
            .collect();
        assert_eq!(
            ports,
            [
                ("dns", 53, "dns", "UDP"),
                ("dns-tcp", 53, "dns-tcp", "TCP"),
                ("metrics", u64::from(METRICS_PORT), "metrics", "TCP"),
            ]
        );
    }

    #[test]
    fn render_container_structure() {
        for rootless in [true, false] {
            let docs = documents(rootless);
            let container = container(&docs);
            assert_eq!(container["name"].as_str(), Some("coredns"));
            let args: Vec<_> = container["args"]
                .as_sequence()
                .unwrap()
                .iter()
                .map(|a| a.as_str().unwrap())
                .collect();
            assert_eq!(args, ["-conf", "/etc/coredns/Corefile"]);
            assert_eq!(
                container["volumeMounts"][0]["mountPath"].as_str(),
                Some("/etc/coredns")
            );
        }
    }

    #[test]
    fn render_resources_only_without_rootless() {
        let docs = documents(false);
        let resources = &container(&docs)["resources"];
        assert_eq!(resources["limits"]["memory"].as_str(), Some("170Mi"));
        assert_eq!(resources["requests"]["cpu"].as_str(), Some("100m"));
        assert_eq!(resources["requests"]["memory"].as_str(), Some("70Mi"));

        let docs = documents(true);
        assert!(container(&docs).get("resources").is_none());
    }
}
