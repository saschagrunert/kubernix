//! Cluster network topology and CIDR allocation.
//!
//! Splits a parent CIDR into the cluster network, which contains one pod
//! network per node, and the service network. Also checks for conflicts
//! with existing system routes and exempts the traffic between the pod
//! networks from masquerading in multi-node mode.

use crate::Config;
use anyhow::{Context, Result, bail};
use ipnetwork::Ipv4Network;
use log::{debug, warn};
use nix::unistd::gethostname;
use std::{
    net::{Ipv4Addr, SocketAddr},
    path::Path,
    process::Command,
};

const ETCD_CLIENT_PORT: u16 = 2379;
const ETCD_PEER_PORT: u16 = 2380;

/// Smallest prefix of the service CIDR. The API server rejects IPv4
/// service CIDRs larger than a /12, and a /16 already provides more
/// ClusterIPs than a local cluster needs.
const SERVICE_CIDR_MIN_PREFIX: u8 = 16;

/// Largest prefix of the cluster, service and pod CIDRs.
const SUBNET_MAX_PREFIX: u8 = 30;

/// Comment prefix of the iptables rule which exempts the cluster CIDR from
/// masquerading.
const MASQUERADE_COMMENT: &str = "kubernix";

#[derive(Clone)]
#[must_use]
pub struct Network {
    cluster_cidr: Ipv4Network,
    pod_cidrs: Vec<Ipv4Network>,
    service_cidr: Ipv4Network,
    multi_node: bool,
    etcd_client: SocketAddr,
    etcd_peer: SocketAddr,
    hostname: String,
}

/// The subnets carved out of the parent CIDR.
#[derive(Debug)]
struct Layout {
    cluster_cidr: Ipv4Network,
    pod_cidrs: Vec<Ipv4Network>,
    service_cidr: Ipv4Network,
}

impl Network {
    /// The global name for the interface
    pub const INTERFACE_PREFIX: &'static str = "kubernix";

    /// Pod network of the whole cluster, which contains the pod CIDRs of
    /// all nodes.
    pub fn cluster_cidr(&self) -> &Ipv4Network {
        &self.cluster_cidr
    }

    /// Per-node pod network subnets, one per configured node.
    pub fn pod_cidrs(&self) -> &[Ipv4Network] {
        &self.pod_cidrs
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

    /// Lowercase system hostname, used as the node name in single-node
    /// mode like the kubelet and kube-proxy do.
    pub fn hostname(&self) -> &str {
        &self.hostname
    }

    /// Calculate the prefix of the pod CIDRs, which is the smallest
    /// prefix (largest subnet) that fits one pod CIDR per node into the
    /// cluster CIDR.
    fn subnet_prefix(cluster_prefix: u8, nodes: u8) -> Result<u8> {
        for prefix in cluster_prefix..=SUBNET_MAX_PREFIX {
            let subnets_available = 1u64 << (prefix - cluster_prefix);
            if subnets_available >= u64::from(nodes) {
                return Ok(prefix);
            }
        }
        bail!(
            "Cluster CIDR /{} is too small to fit {} pod CIDRs",
            cluster_prefix,
            nodes,
        )
    }

    /// Split the parent CIDR in halves: the first one is the cluster CIDR,
    /// which gets split equally into the pod CIDRs of the nodes, the
    /// second one contains the service CIDR, which is at most a /16. Host
    /// bits of the parent CIDR are ignored.
    fn layout(cidr: Ipv4Network, nodes: u8) -> Result<Layout> {
        let half = cidr.prefix() + 1;
        if half > SUBNET_MAX_PREFIX {
            bail!("CIDR {} is too small to be split up", cidr)
        }
        let cluster_cidr = Ipv4Network::new(cidr.network(), half)?;
        let service_cidr = Ipv4Network::new(
            cidr.nth(cluster_cidr.size())
                .context("Unable to retrieve service CIDR start IP")?,
            half.max(SERVICE_CIDR_MIN_PREFIX),
        )?;

        let pod_prefix = Self::subnet_prefix(half, nodes)?;
        let mut pod_cidrs = vec![];
        let mut offset = 0;
        for _ in 0..nodes {
            let cidr = Ipv4Network::new(
                cluster_cidr
                    .nth(offset)
                    .context("Unable to retrieve pod CIDR start IP")?,
                pod_prefix,
            )?;
            offset += cidr.size();
            pod_cidrs.push(cidr);
        }

        Ok(Layout {
            cluster_cidr,
            pod_cidrs,
            service_cidr,
        })
    }

    /// Create a new network from the provided config
    pub fn new(config: &Config) -> Result<Self> {
        let cidr = Ipv4Network::new(config.cidr().network(), config.cidr().prefix())?;
        // layout() fails if the CIDR cannot fit all required subnets
        let layout = Self::layout(cidr, config.nodes())?;
        // In rootless mode, the routes of the host are only visible before
        // entering the network namespace of rootlesskit.
        if !config.is_rootless() {
            Self::warn_overlapping_route(cidr, "cluster CIDR")?;
        }
        debug!("Using cluster CIDR {}", layout.cluster_cidr);
        debug!("Using service CIDR {}", layout.service_cidr);
        for (node, cidr) in layout.pod_cidrs.iter().enumerate() {
            debug!("Using pod ({}) CIDR {}", node, cidr);
        }

        // Set the rest of the networking related adresses and paths
        let etcd_client = SocketAddr::new(Ipv4Addr::LOCALHOST.into(), ETCD_CLIENT_PORT);
        let etcd_peer = SocketAddr::new(Ipv4Addr::LOCALHOST.into(), ETCD_PEER_PORT);
        let hostname = gethostname()
            .context("Unable to get hostname")?
            .to_str()
            .context("Unable to convert hostname into string")?
            .to_lowercase();

        Ok(Self {
            cluster_cidr: layout.cluster_cidr,
            pod_cidrs: layout.pod_cidrs,
            service_cidr: layout.service_cidr,
            multi_node: config.multi_node(),
            etcd_client,
            etcd_peer,
            hostname,
        })
    }

    /// Exempt the traffic within the cluster CIDR from masquerading in
    /// multi-node mode.
    ///
    /// All nodes share one network namespace, where every node has its own
    /// bridge. The masquerading of the bridge plugin only skips the pod
    /// CIDR of its node, so it would also NAT the traffic between pods of
    /// different nodes. The returned rule gets removed again on drop.
    pub fn setup_nat_exemption(&self, root: &Path) -> Result<Option<NatExemption>> {
        if !self.multi_node {
            return Ok(None);
        }
        let exemption = NatExemption::new(self.cluster_cidr, root);
        exemption.setup()?;
        Ok(Some(exemption))
    }

    /// The IPv4 addresses of the network namespace, which the kubelets may
    /// report as node addresses. The addresses of the cluster bridges are
    /// skipped.
    pub fn host_ips() -> Result<Vec<Ipv4Addr>> {
        let output = Command::new("ip")
            .args(["-4", "-o", "addr", "show", "scope", "global"])
            .output()
            .context("Failed to run 'ip addr'; is iproute2 installed?")?;
        if !output.status.success() {
            bail!(
                "'ip addr' exited with status {} (stderr: {})",
                output.status,
                String::from_utf8_lossy(&output.stderr),
            )
        }
        Ok(Self::parse_host_ips(&String::from_utf8(output.stdout)?))
    }

    /// Parse the output of `ip -4 -o addr show`.
    fn parse_host_ips(output: &str) -> Vec<Ipv4Addr> {
        output
            .lines()
            .filter_map(|line| {
                let mut fields = line.split_whitespace().skip(1);
                let interface = fields.next()?;
                if interface.starts_with(Self::INTERFACE_PREFIX) || fields.next()? != "inet" {
                    return None;
                }
                Some(fields.next()?.parse::<Ipv4Network>().ok()?.ip())
            })
            .collect()
    }

    /// Check if there are routes overlapping the named network and warn
    pub fn warn_overlapping_route(cidr: Ipv4Network, name: &str) -> Result<()> {
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
            .filter(|x| x.is_supernet_of(cidr) || cidr.is_supernet_of(*x))
            .for_each(|x| {
                warn!(
                    "Overlapping IP route {} detected ({}: {}), \
                     the cluster may not work as expected",
                    x, name, cidr,
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

    /// Remove the CNI bridge interfaces created by this cluster.
    pub fn cleanup(&self) {
        let output = match Command::new("ip").args(["link", "show"]).output() {
            Ok(o) => o,
            Err(_) => return,
        };
        let stdout = String::from_utf8_lossy(&output.stdout);
        for line in stdout.lines() {
            let name = match line.split(':').nth(1) {
                Some(n) => n.trim(),
                None => continue,
            };
            if name.starts_with(Self::INTERFACE_PREFIX) {
                debug!("Removing network interface {}", name);
                let _ = Command::new("ip").args(["link", "delete", name]).output();
            }
        }
    }
}

/// The iptables rule which exempts the traffic within the cluster CIDR from
/// the masquerading of the CNI bridges, removed again on drop.
///
/// The rule gets appended to the `POSTROUTING` chain before the pods start.
/// So it comes after the rule of kube-proxy, which kube-proxy inserts at the
/// beginning and which still masquerades Service traffic where required,
/// and before the rules of the bridges, which get appended for every pod.
/// Disabling the masquerading of the bridges instead does not work: the
/// bridge plugin then records the MAC address of the host side veth before
/// udev changes it, and its CHECK fails.
#[must_use]
pub struct NatExemption {
    cidr: Ipv4Network,
    comment: String,
}

impl NatExemption {
    /// The rule is identified by the run root in its comment, which is
    /// short because of the unix socket path limits.
    fn new(cidr: Ipv4Network, root: &Path) -> Self {
        Self {
            cidr,
            comment: format!("{} {}", MASQUERADE_COMMENT, root.display()),
        }
    }

    /// The iptables arguments for the provided command (like `-A`) of the
    /// rule for the provided cluster CIDR.
    fn args(&self, cidr: Ipv4Network, command: &str) -> Vec<String> {
        let cidr = cidr.to_string();
        [
            "-w",
            "-t",
            "nat",
            command,
            "POSTROUTING",
            "-s",
            &cidr,
            "-d",
            &cidr,
            "-m",
            "comment",
            "--comment",
            &self.comment,
            "-j",
            "RETURN",
        ]
        .iter()
        .map(ToString::to_string)
        .collect()
    }

    /// Run iptables and return whether it succeeded and its output.
    fn iptables(args: &[String]) -> Result<(bool, String)> {
        let output = Command::new("iptables")
            .args(args)
            .output()
            .context("Failed to run 'iptables'")?;
        debug!(
            "iptables {} exited with {} (stderr: {})",
            args.join(" "),
            output.status,
            String::from_utf8_lossy(&output.stderr).trim(),
        );
        Ok((
            output.status.success(),
            String::from_utf8_lossy(&output.stdout).into(),
        ))
    }

    /// Add the rule if it does not exist yet, after removing the rules of a
    /// previous run for the same root with a different cluster CIDR. Rules
    /// of other roots may belong to running clusters, so they are kept.
    fn setup(&self) -> Result<()> {
        let args = ["-w", "-t", "nat", "-S", "POSTROUTING"].map(String::from);
        let (_, rules) = Self::iptables(&args)?;
        for cidr in self.stale_cidrs(&rules) {
            debug!("Removing stale NAT exemption rule for CIDR {}", cidr);
            Self::iptables(&self.args(cidr, "-D"))?;
        }

        if Self::iptables(&self.args(self.cidr, "-C"))?.0 {
            return Ok(());
        }
        debug!("Exempting cluster CIDR {} from masquerading", self.cidr);
        if !Self::iptables(&self.args(self.cidr, "-A"))?.0 {
            bail!(
                "Unable to add the NAT exemption rule for cluster CIDR {}",
                self.cidr
            )
        }
        Ok(())
    }

    /// The cluster CIDRs of the rules with the comment of this root but a
    /// different CIDR, parsed from the output of `iptables -S`.
    fn stale_cidrs(&self, rules: &str) -> Vec<Ipv4Network> {
        let comment = format!("--comment \"{}\"", self.comment);
        rules
            .lines()
            .filter(|l| l.contains(&comment) && l.ends_with("-j RETURN"))
            .filter_map(|l| {
                let mut fields = l.split_whitespace();
                fields.find(|f| *f == "-s")?;
                fields.next()?.parse::<Ipv4Network>().ok()
            })
            .filter(|cidr| *cidr != self.cidr)
            .collect()
    }
}

impl Drop for NatExemption {
    fn drop(&mut self) {
        debug!("Removing NAT exemption rule for CIDR {}", self.cidr);
        if !matches!(Self::iptables(&self.args(self.cidr, "-D")), Ok((true, _))) {
            debug!("Unable to remove the NAT exemption rule");
        }
    }
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use crate::config::tests::{test_config, test_config_wrong_cidr};
    use clap::Parser;

    pub fn test_network() -> Result<Network> {
        let c = test_config()?;
        Network::new(&c)
    }

    fn layout(cidr: &str, nodes: u8) -> Result<Layout> {
        Network::layout(cidr.parse()?, nodes)
    }

    fn strings(cidrs: &[Ipv4Network]) -> Vec<String> {
        cidrs.iter().map(ToString::to_string).collect()
    }

    /// Check the invariants every layout has to fulfill.
    fn assert_valid(l: &Layout, parent: &str) -> Result<()> {
        let parent: Ipv4Network = parent.parse()?;
        let parent = Ipv4Network::new(parent.network(), parent.prefix())?;
        assert!(parent.is_supernet_of(l.cluster_cidr));
        assert!(parent.is_supernet_of(l.service_cidr));
        assert!(!l.cluster_cidr.overlaps(l.service_cidr));
        assert!(l.service_cidr.prefix() >= SERVICE_CIDR_MIN_PREFIX);
        for (i, pod) in l.pod_cidrs.iter().enumerate() {
            assert_eq!(pod.ip(), pod.network(), "pod CIDR {} has host bits", pod);
            assert!(
                l.cluster_cidr.is_supernet_of(*pod),
                "pod CIDR {} not in cluster CIDR {}",
                pod,
                l.cluster_cidr
            );
            for other in &l.pod_cidrs[i + 1..] {
                assert!(!pod.overlaps(*other), "{} overlaps {}", pod, other);
            }
        }
        Ok(())
    }

    #[test]
    fn new_success() -> Result<()> {
        let c = test_config()?;
        let n = Network::new(&c)?;

        // Default config: 10.10.0.0/16 with 1 node
        assert_eq!(n.cluster_cidr().to_string(), "10.10.0.0/17");
        assert_eq!(n.service_cidr().to_string(), "10.10.128.0/17");
        assert_eq!(strings(n.pod_cidrs()), ["10.10.0.0/17"]);
        assert!(!n.multi_node);
        Ok(())
    }

    #[test]
    fn new_multi_node() -> Result<()> {
        let c = Config::parse_from(["kubernix", "--nodes=3"]);
        let n = Network::new(&c)?;
        assert_eq!(n.cluster_cidr().to_string(), "10.10.0.0/17");
        assert_eq!(
            strings(n.pod_cidrs()),
            ["10.10.0.0/19", "10.10.32.0/19", "10.10.64.0/19"]
        );
        assert!(n.multi_node);
        Ok(())
    }

    #[test]
    fn new_hostname_is_lowercase() -> Result<()> {
        let n = test_network()?;
        assert_eq!(n.hostname(), n.hostname().to_lowercase());
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
        let n = test_network()?;
        assert_eq!(n.api()?, Ipv4Addr::new(10, 10, 128, 1));
        Ok(())
    }

    #[test]
    fn dns_success() -> Result<()> {
        let n = test_network()?;
        assert_eq!(n.dns()?, Ipv4Addr::new(10, 10, 128, 2));
        Ok(())
    }

    #[test]
    fn layout_multi_node() -> Result<()> {
        for nodes in 1..=10 {
            let l = layout("10.10.0.0/16", nodes)?;
            assert_valid(&l, "10.10.0.0/16")?;
            assert_eq!(l.pod_cidrs.len(), usize::from(nodes));
        }
        let l = layout("10.10.0.0/16", 5)?;
        assert_eq!(
            strings(&l.pod_cidrs),
            [
                "10.10.0.0/20",
                "10.10.16.0/20",
                "10.10.32.0/20",
                "10.10.48.0/20",
                "10.10.64.0/20",
            ]
        );
        assert_eq!(l.service_cidr.to_string(), "10.10.128.0/17");
        Ok(())
    }

    #[test]
    fn layout_unaligned_input() -> Result<()> {
        let l = layout("10.10.1.1/16", 2)?;
        assert_valid(&l, "10.10.1.1/16")?;
        assert_eq!(l.cluster_cidr.to_string(), "10.10.0.0/17");
        assert_eq!(l.service_cidr.to_string(), "10.10.128.0/17");
        assert_eq!(strings(&l.pod_cidrs), ["10.10.0.0/18", "10.10.64.0/18"]);
        Ok(())
    }

    #[test]
    fn layout_large_parent() -> Result<()> {
        let l = layout("10.0.0.0/8", 3)?;
        assert_valid(&l, "10.0.0.0/8")?;
        assert_eq!(l.cluster_cidr.to_string(), "10.0.0.0/9");
        assert_eq!(l.service_cidr.to_string(), "10.128.0.0/16");
        assert_eq!(
            strings(&l.pod_cidrs),
            ["10.0.0.0/11", "10.32.0.0/11", "10.64.0.0/11"]
        );
        Ok(())
    }

    #[test]
    fn layout_service_cidr_size() -> Result<()> {
        for (parent, service) in [
            ("10.0.0.0/8", "10.128.0.0/16"),
            ("172.16.0.0/12", "172.24.0.0/16"),
            ("10.10.0.0/15", "10.11.0.0/16"),
            ("10.10.0.0/16", "10.10.128.0/17"),
            ("10.10.0.0/24", "10.10.0.128/25"),
        ] {
            let l = layout(parent, 1)?;
            assert_valid(&l, parent)?;
            assert_eq!(l.service_cidr.to_string(), service);
        }
        Ok(())
    }

    #[test]
    fn layout_pod_cidrs_in_cluster_cidr() -> Result<()> {
        for parent in [
            "10.0.0.0/8",
            "10.10.0.0/16",
            "192.168.0.0/24",
            "10.10.1.1/20",
        ] {
            for nodes in [1, 2, 3, 7, 8, 9] {
                assert_valid(&layout(parent, nodes)?, parent)?;
            }
        }
        Ok(())
    }

    #[test]
    fn layout_too_small() {
        assert!(layout("10.0.0.0/30", 1).is_err());
        assert!(layout("10.0.0.0/29", 1).is_ok());
        assert!(layout("10.0.0.0/29", 2).is_err());
        assert!(layout("10.0.0.0/28", 2).is_ok());
    }

    #[test]
    fn subnet_prefix_single_node() -> Result<()> {
        assert_eq!(Network::subnet_prefix(17, 1)?, 17);
        Ok(())
    }

    #[test]
    fn subnet_prefix_many_nodes() -> Result<()> {
        assert_eq!(Network::subnet_prefix(17, 2)?, 18);
        assert_eq!(Network::subnet_prefix(17, 6)?, 20);
        assert_eq!(Network::subnet_prefix(17, 8)?, 20);
        assert_eq!(Network::subnet_prefix(17, 255)?, 25);
        Ok(())
    }

    #[test]
    fn subnet_prefix_too_small() {
        assert!(Network::subnet_prefix(30, 2).is_err());
    }

    #[test]
    fn nat_exemption_rule_for_cluster_cidr() -> Result<()> {
        let m = NatExemption::new("10.10.0.0/17".parse()?, Path::new("/run/kx"));
        let args = m.args(m.cidr, "-A");
        // Do not run iptables on drop
        std::mem::forget(m);
        assert_eq!(
            args.join(" "),
            "-w -t nat -A POSTROUTING -s 10.10.0.0/17 -d 10.10.0.0/17 \
             -m comment --comment kubernix /run/kx -j RETURN"
        );
        Ok(())
    }

    #[test]
    fn nat_exemption_stale_cidrs_of_same_root() -> Result<()> {
        let m = NatExemption::new("10.10.0.0/17".parse()?, Path::new("/run/kx"));
        let rules = concat!(
            "-P POSTROUTING ACCEPT\n",
            "-A POSTROUTING -m comment --comment \"kubernetes postrouting rules\" ",
            "-j KUBE-POSTROUTING\n",
            "-A POSTROUTING -s 10.10.0.0/17 -d 10.10.0.0/17 ",
            "-m comment --comment \"kubernix /run/kx\" -j RETURN\n",
            "-A POSTROUTING -s 10.20.0.0/17 -d 10.20.0.0/17 ",
            "-m comment --comment \"kubernix /run/kx\" -j RETURN\n",
            "-A POSTROUTING -s 10.30.0.0/17 -d 10.30.0.0/17 ",
            "-m comment --comment \"kubernix /run/other\" -j RETURN\n",
        );
        let stale = m.stale_cidrs(rules);
        std::mem::forget(m);
        assert_eq!(strings(&stale), ["10.20.0.0/17"]);
        Ok(())
    }

    #[test]
    fn parse_host_ips_skips_bridges() {
        let output = concat!(
            "2: enp0s31f6    inet 192.168.1.10/24 brd 192.168.1.255 scope global enp0s31f6\\\n",
            "3: kubernix.0    inet 10.10.0.1/19 brd 10.10.31.255 scope global kubernix.0\\\n",
            "4: tap0    inet 10.0.2.100/24 brd 10.0.2.255 scope global tap0\\\n",
        );
        assert_eq!(
            Network::parse_host_ips(output),
            [Ipv4Addr::new(192, 168, 1, 10), Ipv4Addr::new(10, 0, 2, 100)]
        );
    }
}
