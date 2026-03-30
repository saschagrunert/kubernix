//! Cluster network topology and CIDR allocation.
//!
//! Splits a parent CIDR into non-overlapping subnets for the cluster
//! network, service network, and per-node CRI-O pod networks. Also
//! checks for conflicts with existing system routes.

use crate::Config;
use anyhow::{Context, Result, bail};
use hostname::get;
use ipnetwork::Ipv4Network;
use log::{debug, warn};
use std::{
    net::{Ipv4Addr, SocketAddr},
    process::Command,
};

#[derive(Clone)]
#[must_use]
pub struct Network {
    cluster_cidr: Ipv4Network,
    crio_cidrs: Vec<Ipv4Network>,
    service_cidr: Ipv4Network,
    etcd_client: SocketAddr,
    etcd_peer: SocketAddr,
    hostname: String,
}

impl Network {
    /// The global name for the interface
    pub const INTERFACE_PREFIX: &'static str = "kubernix";

    /// Subnet used for pod-to-pod communication across the cluster.
    pub fn cluster_cidr(&self) -> &Ipv4Network {
        &self.cluster_cidr
    }

    /// Per-node CRI-O pod network subnets, one per configured node.
    pub fn crio_cidrs(&self) -> &[Ipv4Network] {
        &self.crio_cidrs
    }

    /// Subnet reserved for Kubernetes Service ClusterIPs.
    pub fn service_cidr(&self) -> &Ipv4Network {
        &self.service_cidr
    }

    /// etcd client endpoint (localhost:2379).
    pub fn etcd_client(&self) -> &SocketAddr {
        &self.etcd_client
    }

    /// etcd peer endpoint (localhost:2380).
    pub fn etcd_peer(&self) -> &SocketAddr {
        &self.etcd_peer
    }

    /// System hostname used as the node name in single-node mode.
    pub fn hostname(&self) -> &str {
        &self.hostname
    }

    /// Calculate the subnet prefix to use for splitting the parent CIDR.
    ///
    /// We need (2 + nodes) subnets: cluster, service, and one per CRI-O node.
    /// The prefix is chosen so that all subnets fit inside the parent CIDR.
    fn subnet_prefix(parent_prefix: u8, nodes: u8) -> Result<u8> {
        let required = 2 + u32::from(nodes); // cluster + service + N nodes
        // Find the smallest prefix (largest subnet) that still fits
        for prefix in parent_prefix..=30 {
            let subnets_available = 1u32 << (prefix - parent_prefix);
            if subnets_available >= required {
                return Ok(prefix);
            }
        }
        bail!(
            "CIDR /{} is too small to fit {} required subnets",
            parent_prefix,
            required,
        )
    }

    /// Create a new network from the provided config
    pub fn new(config: &Config) -> Result<Self> {
        // subnet_prefix() fails if the CIDR cannot fit all required subnets
        let subnet_prefix = Self::subnet_prefix(config.cidr().prefix(), config.nodes())?;
        Self::warn_overlapping_route(config.cidr())?;

        // Calculate the CIDRs using the dynamic subnet prefix
        let cluster_cidr = Ipv4Network::new(config.cidr().ip(), subnet_prefix)?;
        debug!("Using cluster CIDR {}", cluster_cidr);

        let service_cidr = Ipv4Network::new(
            config
                .cidr()
                .nth(cluster_cidr.size())
                .context("Unable to retrieve service CIDR start IP")?,
            subnet_prefix,
        )?;
        debug!("Using service CIDR {}", service_cidr);

        let mut crio_cidrs = vec![];
        let mut offset = cluster_cidr.size() + service_cidr.size();
        for node in 0..config.nodes() {
            let cidr = Ipv4Network::new(
                config
                    .cidr()
                    .nth(offset)
                    .context("Unable to retrieve CRI-O CIDR start IP")?,
                subnet_prefix,
            )?;
            offset += cidr.size();
            debug!("Using CRI-O ({}) CIDR {}", node, cidr);
            crio_cidrs.push(cidr);
        }

        // Set the rest of the networking related adresses and paths
        let etcd_client = SocketAddr::new(Ipv4Addr::LOCALHOST.into(), 2379);
        let etcd_peer = SocketAddr::new(Ipv4Addr::LOCALHOST.into(), 2380);
        let hostname = get()
            .context("Unable to get hostname")?
            .to_str()
            .context("Unable to convert hostname into string")?
            .into();

        Ok(Self {
            cluster_cidr,
            crio_cidrs,
            service_cidr,
            etcd_client,
            etcd_peer,
            hostname,
        })
    }

    /// Check if there are overlapping routes and warn
    fn warn_overlapping_route(cidr: Ipv4Network) -> Result<()> {
        let cmd = Command::new("ip")
            .arg("route")
            .output()
            .context("Failed to run 'ip route'; is iproute2 installed?")?;
        if !cmd.status.success() {
            bail!(
                "'ip route' exited with status {} (stderr: {})",
                cmd.status,
                String::from_utf8_lossy(&cmd.stderr),
            )
        }
        String::from_utf8(cmd.stdout)?
            .lines()
            .filter(|x| !x.contains(Self::INTERFACE_PREFIX))
            .filter_map(|x| x.split_whitespace().next())
            .filter_map(|x| x.parse::<Ipv4Network>().ok())
            .filter(|x| x.is_supernet_of(cidr))
            .for_each(|x| {
                warn!(
                    "There seems to be an overlapping IP route {}. {}",
                    x, "the cluster may not work as expected",
                );
            });
        Ok(())
    }

    /// Retrieve the API server address from the service CIDR
    pub fn api(&self) -> Result<Ipv4Addr> {
        self.service_cidr().nth(1).with_context(|| {
            format!(
                "Unable to retrieve first IP from service CIDR: {}",
                self.service_cidr()
            )
        })
    }

    /// Retrieve the DNS address from the service CIDR
    pub fn dns(&self) -> Result<Ipv4Addr> {
        self.service_cidr().nth(2).with_context(|| {
            format!(
                "Unable to retrieve second IP from service CIDR: {}",
                self.service_cidr()
            )
        })
    }
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use crate::config::tests::{test_config, test_config_wrong_cidr};

    pub fn test_network() -> Result<Network> {
        let c = test_config()?;
        Network::new(&c)
    }

    #[test]
    fn new_success() -> Result<()> {
        let c = test_config()?;
        let n = Network::new(&c)?;

        // Default config: 10.10.0.0/16 with 1 node -> /18 subnets
        assert_eq!(
            n.cluster_cidr().to_string(),
            "10.10.0.0/18",
            "cluster CIDR should be the first /18 subnet"
        );
        assert_eq!(
            n.service_cidr().to_string(),
            "10.10.64.0/18",
            "service CIDR should be the second /18 subnet"
        );
        assert_eq!(
            n.crio_cidrs().len(),
            1,
            "single node should have one CRI-O CIDR"
        );
        assert_eq!(
            n.crio_cidrs()[0].to_string(),
            "10.10.128.0/18",
            "CRI-O CIDR should be the third /18 subnet"
        );
        Ok(())
    }

    #[test]
    fn new_failure() -> Result<()> {
        let c = test_config_wrong_cidr()?;
        assert!(Network::new(&c).is_err());
        Ok(())
    }

    #[test]
    fn api_success() -> Result<()> {
        let c = test_config()?;
        let n = Network::new(&c)?;
        // service CIDR = 10.10.64.0/18, api = nth(1)
        assert_eq!(n.api()?, Ipv4Addr::new(10, 10, 64, 1));
        Ok(())
    }

    #[test]
    fn dns_success() -> Result<()> {
        let c = test_config()?;
        let n = Network::new(&c)?;
        // service CIDR = 10.10.64.0/18, dns = nth(2)
        assert_eq!(n.dns()?, Ipv4Addr::new(10, 10, 64, 2));
        Ok(())
    }

    #[test]
    fn subnet_prefix_single_node() -> Result<()> {
        // 1 node needs 3 subnets (cluster + service + 1 node)
        // From /16: need 2 bits -> /18
        assert_eq!(Network::subnet_prefix(16, 1)?, 18);
        Ok(())
    }

    #[test]
    fn subnet_prefix_many_nodes() -> Result<()> {
        // 6 nodes needs 8 subnets (cluster + service + 6 nodes)
        // From /16: need 3 bits -> /19
        assert_eq!(Network::subnet_prefix(16, 6)?, 19);
        Ok(())
    }

    #[test]
    fn subnet_prefix_too_small() {
        // /30 can hold 4 IPs; need 3 subnets, impossible
        assert!(Network::subnet_prefix(30, 1).is_err());
    }
}
