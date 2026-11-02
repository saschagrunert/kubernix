//! Cluster network topology and CIDR allocation.
//!
//! Splits a parent CIDR into the cluster network, which contains one pod
//! network per node, and the service network. Also checks for conflicts
//! with existing system routes and ports, manages the CNI bridges of the
//! nodes and exempts the traffic between the pod networks from
//! masquerading in multi-node mode.

use crate::{API_SERVER_PORT, Config, process, system::System};
use anyhow::{Context, Result, bail};
use ipnetwork::Ipv4Network;
use log::{debug, warn};
use nix::unistd::gethostname;
use std::{
    ffi::OsStr,
    fs::read_to_string,
    io::{self, ErrorKind},
    net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener},
    path::{Path, PathBuf},
};

const ETCD_CLIENT_PORT: u16 = 2379;
const ETCD_PEER_PORT: u16 = 2380;

/// Ports of the kubelet API and healthz endpoint of the first node, every
/// further node uses the next port.
pub(crate) const KUBELET_PORT_BASE: u16 = 11250;
pub(crate) const HEALTHZ_PORT_BASE: u16 = 12250;

/// Prefix of the CNI bridge names. The run root id and the node number
/// follow, like `kx0123abcd.0`, so that the clusters of different run roots
/// do not share bridges. The name has to fit into IFNAMSIZ (15 characters).
const BRIDGE_PREFIX: &str = "kx";

/// Prefix of the CNI bridges of former versions, which are skipped like
/// the current ones when looking up the host addresses, and removed like
/// the ones of other run roots if they overlap the CIDR.
const LEGACY_BRIDGE_PREFIX: &str = "kubernix";

/// The resolver configuration of systemd-resolved with the upstream
/// servers, used instead of its stub resolver on a loopback address.
pub(crate) const SYSTEMD_RESOLV_CONF: &str = "/run/systemd/resolve/resolv.conf";

/// The default resolver configuration.
const RESOLV_CONF: &str = "/etc/resolv.conf";

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
    cidr: Ipv4Network,
    cluster_cidr: Ipv4Network,
    pod_cidrs: Vec<Ipv4Network>,
    service_cidr: Ipv4Network,
    multi_node: bool,
    etcd_client: SocketAddr,
    etcd_peer: SocketAddr,
    hostname: String,
    bridge_prefix: String,
    rootless: bool,
}

/// The subnets carved out of the parent CIDR.
#[derive(Debug)]
struct Layout {
    cluster_cidr: Ipv4Network,
    pod_cidrs: Vec<Ipv4Network>,
    service_cidr: Ipv4Network,
}

impl Network {
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

    /// Check that the CIDR can be split up for the nodes.
    pub(crate) fn check_layout(cidr: Ipv4Network, nodes: u8) -> Result<()> {
        Self::layout(cidr, nodes).map(|_| ())
    }

    /// Create a new network from the provided config
    pub fn new(config: &Config) -> Result<Self> {
        let cidr = Ipv4Network::new(config.cidr().network(), config.cidr().prefix())?;
        // layout() fails if the CIDR cannot fit all required subnets
        let layout = Self::layout(cidr, config.nodes())?;
        debug!("Using cluster CIDR {}", layout.cluster_cidr);
        debug!("Using service CIDR {}", layout.service_cidr);
        for (node, cidr) in layout.pod_cidrs.iter().enumerate() {
            debug!("Using pod ({}) CIDR {}", node, cidr);
        }

        // Set the rest of the networking related adresses and paths
        let etcd_client = SocketAddr::new(Ipv4Addr::LOCALHOST.into(), ETCD_CLIENT_PORT);
        let etcd_peer = SocketAddr::new(Ipv4Addr::LOCALHOST.into(), ETCD_PEER_PORT);
        let hostname = Self::node_hostname(&gethostname().context("Unable to get hostname")?)?;

        Ok(Self {
            cidr,
            cluster_cidr: layout.cluster_cidr,
            pod_cidrs: layout.pod_cidrs,
            service_cidr: layout.service_cidr,
            multi_node: config.multi_node(),
            etcd_client,
            etcd_peer,
            hostname,
            bridge_prefix: format!("{}{}.", BRIDGE_PREFIX, System::root_id(config.root())),
            rootless: config.is_rootless(),
        })
    }

    /// Name of the CNI bridge of the node.
    pub fn bridge(&self, node: u8) -> String {
        format!("{}{}", self.bridge_prefix, node)
    }

    /// The MAC address of the CNI bridge of the node: locally administered
    /// and unicast, followed by the run root id and the node number.
    fn bridge_mac(&self, node: u8) -> String {
        let id = self
            .bridge_prefix
            .trim_start_matches(BRIDGE_PREFIX)
            .trim_end_matches('.');
        let mut mac = String::from("02");
        for i in (0..id.len()).step_by(2) {
            mac.push(':');
            mac.push_str(&id[i..i + 2]);
        }
        format!("{}:{:02x}", mac, node)
    }

    /// Whether the interface is a CNI bridge of any kubernix cluster.
    fn is_bridge(name: &str) -> bool {
        if name.starts_with(LEGACY_BRIDGE_PREFIX) {
            return true;
        }
        let Some((id, node)) = name
            .strip_prefix(BRIDGE_PREFIX)
            .and_then(|x| x.split_once('.'))
        else {
            return false;
        };
        id.len() == 8
            && id.bytes().all(|b| b.is_ascii_hexdigit())
            && !node.is_empty()
            && node.bytes().all(|b| b.is_ascii_digit())
    }

    /// The gateway address of the bridge of the node with the prefix of
    /// its pod CIDR. It is the first address of the pod CIDR, which the
    /// host-local IPAM plugin uses by default.
    fn gateway(&self, node: u8) -> Result<Ipv4Network> {
        let cidr = self
            .pod_cidrs
            .get(usize::from(node))
            .with_context(|| format!("Unable to find the pod CIDR of node {}", node))?;
        let ip = cidr
            .nth(1)
            .with_context(|| format!("Unable to retrieve the gateway of pod CIDR {}", cidr))?;
        Ok(Ipv4Network::new(ip, cidr.prefix())?)
    }

    /// The IP which the kubelet of the node reports as InternalIP, if set.
    ///
    /// In root mode, it is the gateway address of the bridge of the node,
    /// which kubernix creates before the kubelets start. So it does not
    /// change with the network of the host, for example on a new DHCP
    /// lease, and host network pods like CoreDNS do not listen on the
    /// addresses of the host, which other machines can reach. In rootless
    /// mode, the network namespace of rootlesskit has only one address,
    /// which the kubelet picks by default and which is not reachable from
    /// outside.
    pub fn node_ip(&self, node: u8) -> Result<Option<Ipv4Addr>> {
        if self.rootless {
            return Ok(None);
        }
        Ok(Some(self.gateway(node)?.ip()))
    }

    /// The node IPs of all nodes, see `node_ip`.
    pub fn node_ips(&self) -> Result<Vec<Ipv4Addr>> {
        let mut ips = vec![];
        for node in 0..self.pod_cidrs.len() {
            ips.extend(self.node_ip(u8::try_from(node)?)?);
        }
        Ok(ips)
    }

    /// The IPs which the kubelets may report as InternalIP, which their
    /// serving certificates have to contain. In root mode, these are the
    /// node IPs, which do not change with the network of the host. In
    /// rootless mode, the kubelets pick an address of the network
    /// namespace of rootlesskit.
    pub fn kubelet_ips(&self) -> Result<Vec<Ipv4Addr>> {
        if self.rootless {
            Self::host_ips()
        } else {
            self.node_ips()
        }
    }

    /// The address which the API server advertises to the cluster members
    /// as endpoint of the `kubernetes` Service, if set. In root mode, it is
    /// the node IP of the first node, so that it does not change with the
    /// network of the host and the API server also starts without a
    /// default route. In rootless mode, the API server picks the address
    /// of the network namespace of rootlesskit, which has a default route
    /// via slirp4netns.
    pub fn advertise_address(&self) -> Result<Option<Ipv4Addr>> {
        self.node_ip(0)
    }

    /// Remove the bridges which a former run of the same root left behind,
    /// for example after a force quit, since they may have a different
    /// gateway address. No other instance uses them, since the run root is
    /// locked.
    ///
    /// In root mode, also remove the bridges of other run roots and former
    /// versions whose addresses overlap the CIDR, since the kernel may
    /// route the pod traffic via them. They belong to clusters which did
    /// not clean up, for example after a force quit, since the lock of the
    /// host state (see `System::setup`) ensures before that no other
    /// cluster runs, starts or stops in root mode. Pods of these clusters
    /// which are still attached lose their network. Then create the bridges
    /// of the nodes with their gateway address and a stable MAC address, so
    /// that the gateway can be used as node IP before the first pod starts.
    /// The CNI bridge plugin uses the existing bridges.
    ///
    /// The returned guard removes the bridges of the run root again on
    /// drop.
    pub fn setup_bridges(&self) -> Result<Bridges> {
        let bridges = Bridges {
            prefix: self.bridge_prefix.clone(),
        };
        bridges.remove();
        if !self.rootless {
            self.remove_overlapping_bridges();
            // In rootless mode, the routes of the host are only visible
            // before entering the network namespace of rootlesskit, where
            // they get checked instead.
            Self::warn_overlapping_route(self.cidr, "cluster CIDR", Some(&self.bridge_prefix))?;
            for node in 0..self.pod_cidrs.len() {
                self.create_bridge(u8::try_from(node)?)?;
            }
        }
        Ok(bridges)
    }

    /// Remove the bridges of other run roots and former versions which
    /// overlap the CIDR, see `setup_bridges`.
    fn remove_overlapping_bridges(&self) {
        let output = match process::command("ip")
            .args(["-4", "-o", "addr", "show", "type", "bridge"])
            .output()
        {
            Ok(o) => o,
            Err(e) => {
                debug!("Unable to list the bridge addresses: {}", e);
                return;
            }
        };
        for name in self.overlapping_bridges(&String::from_utf8_lossy(&output.stdout)) {
            warn!(
                "Removing bridge {} of another run directory or a former version, \
                 whose address overlaps the CIDR {}",
                name, self.cidr,
            );
            Bridges::delete(&name);
        }
    }

    /// The CNI bridges of other run roots and former versions in the output
    /// of `ip -4 -o addr show`, which have an address overlapping the CIDR.
    fn overlapping_bridges(&self, output: &str) -> Vec<String> {
        let mut names: Vec<String> = output
            .lines()
            .filter_map(|line| {
                let mut fields = line.split_whitespace().skip(1);
                let name = fields.next()?;
                if !Self::is_bridge(name)
                    || name.starts_with(&self.bridge_prefix)
                    || fields.next()? != "inet"
                {
                    return None;
                }
                let address = fields.next()?.parse::<Ipv4Network>().ok()?;
                let address = Ipv4Network::new(address.network(), address.prefix()).ok()?;
                address.overlaps(self.cidr).then(|| name.to_string())
            })
            .collect();
        names.dedup();
        names
    }

    fn create_bridge(&self, node: u8) -> Result<()> {
        let name = self.bridge(node);
        let gateway = self.gateway(node)?.to_string();
        // The CNI bridge plugin only sets the MAC address of a bridge to
        // which it adds the address. Otherwise, the bridge takes the lowest
        // MAC address of its ports and drops to zero once the last port
        // leaves, which makes the ARP entries of the pods stale.
        let mac = self.bridge_mac(node);
        debug!(
            "Creating bridge {} with address {} and MAC address {}",
            name, gateway, mac
        );
        for args in [
            &["link", "add", &name, "address", &mac, "type", "bridge"][..],
            &["addr", "add", &gateway, "dev", &name],
            &["link", "set", "dev", &name, "up"],
        ] {
            let output = process::command("ip")
                .args(args)
                .output()
                .context("Failed to run 'ip'; is iproute2 installed?")?;
            if !output.status.success() {
                bail!(
                    "Unable to create bridge {}, 'ip {}' failed: {}",
                    name,
                    args.join(" "),
                    String::from_utf8_lossy(&output.stderr).trim(),
                )
            }
        }
        Ok(())
    }

    /// The addresses the components listen on, with their names.
    fn listen_addresses(&self, nodes: u8) -> Vec<(SocketAddr, String)> {
        let any = Ipv4Addr::UNSPECIFIED;
        let localhost = Ipv4Addr::LOCALHOST;
        let mut addresses = vec![
            (self.etcd_client, "etcd".to_string()),
            (self.etcd_peer, "etcd".into()),
            ((any, API_SERVER_PORT).into(), "the API server".into()),
            ((localhost, 10257).into(), "the controller manager".into()),
            ((any, 10259).into(), "the scheduler".into()),
            ((localhost, 10249).into(), "kube-proxy".into()),
            ((any, 10256).into(), "kube-proxy".into()),
        ];
        for node in 0..nodes {
            let offset = u16::from(node);
            let kubelet = format!("the kubelet of node {}", node);
            addresses.push(((any, KUBELET_PORT_BASE + offset).into(), kubelet.clone()));
            addresses.push(((localhost, HEALTHZ_PORT_BASE + offset).into(), kubelet));
        }
        addresses
    }

    /// Fail if a port of the components is already in use, for example by
    /// the cluster of another run root, before changing the system.
    pub fn check_ports(&self, nodes: u8) -> Result<()> {
        for (address, name) in self.listen_addresses(nodes) {
            match Self::port_in_use(address, &format!("of {}", name)) {
                Ok(None) => {}
                Ok(Some(message)) => bail!(message),
                Err(e) => debug!("Unable to check the address {}: {}", address, e),
            }
        }
        Ok(())
    }

    /// Check if `address` can be bound, which fails if another process
    /// listens on it. Returns the message for a port which is in use, where
    /// `user` describes what needs it, or the error of a failed check.
    pub fn port_in_use(address: SocketAddr, user: &str) -> io::Result<Option<String>> {
        match TcpListener::bind(address) {
            Ok(_) => Ok(None),
            Err(e) if e.kind() == ErrorKind::AddrInUse => Ok(Some(format!(
                "Port {} {} is already in use. Stop the process using it, for example \
                 the cluster of another run directory, which 'kubernix status --root \
                 <dir>' shows and 'kubernix stop --root <dir>' stops",
                address.port(),
                user,
            ))),
            Err(e) => Err(e),
        }
    }

    /// The resolver configuration for pods with the `Default` DNS policy.
    /// The stub resolver of systemd-resolved listens on a loopback address,
    /// which pods cannot reach, so its configuration with the upstream
    /// servers is used instead, like kubeadm does.
    pub fn resolv_conf() -> PathBuf {
        let stub = read_to_string(RESOLV_CONF)
            .map(|x| Self::has_loopback_nameserver(&x))
            .unwrap_or_default();
        if stub && Path::new(SYSTEMD_RESOLV_CONF).exists() {
            SYSTEMD_RESOLV_CONF.into()
        } else {
            RESOLV_CONF.into()
        }
    }

    /// Whether the resolver configuration contains a loopback nameserver.
    fn has_loopback_nameserver(content: &str) -> bool {
        content.lines().any(|line| {
            let mut fields = line.split_whitespace();
            fields.next() == Some("nameserver")
                && fields
                    .next()
                    .and_then(|x| x.parse::<IpAddr>().ok())
                    .is_some_and(|x| x.is_loopback())
        })
    }

    /// The node name for the host name, which Kubernetes requires to be
    /// lowercase.
    fn node_hostname(hostname: &OsStr) -> Result<String> {
        Ok(hostname
            .to_str()
            .context("Unable to convert hostname into string")?
            .to_lowercase())
    }

    /// Exempt the traffic within the cluster CIDR from masquerading in
    /// multi-node mode.
    ///
    /// All nodes share one network namespace, where every node has its own
    /// bridge. The masquerading of the bridge plugin only skips the pod
    /// CIDR of its node, so it would also NAT the traffic between pods of
    /// different nodes. The returned rule gets removed again on drop.
    ///
    /// In root mode, the stale rules of former runs get removed in any
    /// case, see `NatExemption::remove_stale`.
    pub fn setup_nat_exemption(&self, root: &Path) -> Result<Option<NatExemption>> {
        let comment = NatExemption::comment(root);
        if !self.multi_node {
            if !self.rootless {
                NatExemption::remove_stale(None, &comment)?;
            }
            return Ok(None);
        }
        let exemption = NatExemption {
            cidr: self.cluster_cidr,
            comment,
        };
        exemption.setup()?;
        Ok(Some(exemption))
    }

    /// The IPv4 addresses of the network namespace, which the kubelets may
    /// report as node addresses. The addresses of the cluster bridges are
    /// skipped.
    pub fn host_ips() -> Result<Vec<Ipv4Addr>> {
        let output = process::command("ip")
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
                if Self::is_bridge(interface) || fields.next()? != "inet" {
                    return None;
                }
                Some(fields.next()?.parse::<Ipv4Network>().ok()?.ip())
            })
            .collect()
    }

    /// Check if there are routes overlapping the named network and warn,
    /// except the ones of the bridges with the `own` prefix. Host bits of
    /// the CIDR are ignored.
    pub fn warn_overlapping_route(cidr: Ipv4Network, name: &str, own: Option<&str>) -> Result<()> {
        let cidr = Ipv4Network::new(cidr.network(), cidr.prefix())?;
        let cmd = process::command("ip")
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
        for route in Self::overlapping_routes(&String::from_utf8(cmd.stdout)?, cidr, own) {
            warn!(
                "Overlapping IP route {} detected ({}: {}), \
                 the cluster may not work as expected",
                route, name, cidr,
            );
        }
        Ok(())
    }

    /// The routes in the output of `ip route` which overlap the CIDR,
    /// except the ones of the bridges with the `own` prefix.
    fn overlapping_routes(output: &str, cidr: Ipv4Network, own: Option<&str>) -> Vec<Ipv4Network> {
        output
            .lines()
            .filter(|line| {
                let mut fields = line.split_whitespace();
                fields.find(|f| *f == "dev");
                !fields
                    .next()
                    .zip(own)
                    .is_some_and(|(dev, own)| dev.starts_with(own))
            })
            .filter_map(|line| line.split_whitespace().next()?.parse::<Ipv4Network>().ok())
            .filter(|route| route.overlaps(cidr))
            .collect()
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

/// The CNI bridges of the run root, removed on drop.
#[must_use]
pub struct Bridges {
    prefix: String,
}

impl Bridges {
    /// Remove all bridges of the run root, also the ones of nodes which do
    /// not exist anymore. The bridges of other run roots are kept here:
    /// they do not affect this cluster unless they overlap its CIDR (see
    /// `Network::setup_bridges`), and the next start of their root removes
    /// them.
    fn remove(&self) {
        let output = match process::command("ip")
            .args(["-o", "link", "show", "type", "bridge"])
            .output()
        {
            Ok(o) => o,
            Err(e) => {
                debug!("Unable to list the bridges: {}", e);
                return;
            }
        };
        for name in Self::names(&String::from_utf8_lossy(&output.stdout), &self.prefix) {
            debug!("Removing bridge {}", name);
            Self::delete(&name);
        }
    }

    /// Delete the bridge, which detaches its ports.
    fn delete(name: &str) {
        match process::command("ip")
            .args(["link", "delete", name])
            .output()
        {
            Ok(o) if o.status.success() => {}
            _ => warn!("Unable to remove bridge {}, may need manual cleanup", name),
        }
    }

    /// The interface names with the prefix in the output of `ip -o link`.
    fn names(output: &str, prefix: &str) -> Vec<String> {
        output
            .lines()
            .filter_map(|line| line.split(':').nth(1))
            .map(str::trim)
            .filter(|name| name.starts_with(prefix))
            .map(String::from)
            .collect()
    }
}

impl Drop for Bridges {
    fn drop(&mut self) {
        self.remove();
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
    /// The rule is identified by the full path of the run root in its
    /// comment, so that the rules of removed run roots can be found (see
    /// `remove_stale`). The path fits into the 255 characters which the
    /// iptables comment match allows, since the unix sockets below the run
    /// root limit its length (see `Kubelet::validate_paths`).
    fn comment(root: &Path) -> String {
        format!("{} {}", MASQUERADE_COMMENT, root.display())
    }

    /// The iptables arguments for the provided command (like `-A`) of the
    /// rule for the provided cluster CIDR and comment.
    fn args(cidr: Ipv4Network, comment: &str, command: &str) -> Vec<String> {
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
            comment,
            "-j",
            "RETURN",
        ]
        .iter()
        .map(ToString::to_string)
        .collect()
    }

    /// Run iptables and return whether it succeeded and its output.
    fn iptables(args: &[String]) -> Result<(bool, String)> {
        let output = process::command("iptables")
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

    /// Add the rule if it does not exist yet, after removing the stale
    /// rules.
    fn setup(&self) -> Result<()> {
        Self::remove_stale(Some(self.cidr), &self.comment)?;
        if Self::iptables(&Self::args(self.cidr, &self.comment, "-C"))?.0 {
            return Ok(());
        }
        debug!("Exempting cluster CIDR {} from masquerading", self.cidr);
        if !Self::iptables(&Self::args(self.cidr, &self.comment, "-A"))?.0 {
            bail!(
                "Unable to add the NAT exemption rule for cluster CIDR {}",
                self.cidr
            )
        }
        Ok(())
    }

    /// Remove the rules which former runs left behind, for example after a
    /// force quit: the rules with the `comment` of this root, except the
    /// one for the `keep` CIDR, and the rules of other roots which do not
    /// exist anymore. No other cluster runs in root mode at this point (see
    /// `System::setup`), but the rules of other existing roots are
    /// kept like their bridges: they only skip the masquerading within
    /// their CIDR, and the next start of their root removes them.
    fn remove_stale(keep: Option<Ipv4Network>, comment: &str) -> Result<()> {
        let args = ["-w", "-t", "nat", "-S", "POSTROUTING"].map(String::from);
        let (_, rules) = Self::iptables(&args)?;
        for (cidr, comment) in Self::stale_rules(&rules, keep, comment, |x| x.exists()) {
            debug!(
                "Removing stale NAT exemption rule for CIDR {} ({})",
                cidr, comment
            );
            Self::iptables(&Self::args(cidr, &comment, "-D"))?;
        }
        Ok(())
    }

    /// The CIDRs and comments of the stale rules (see `remove_stale`),
    /// parsed from the output of `iptables -S`.
    fn stale_rules<F>(
        rules: &str,
        keep: Option<Ipv4Network>,
        comment: &str,
        root_exists: F,
    ) -> Vec<(Ipv4Network, String)>
    where
        F: Fn(&Path) -> bool,
    {
        let prefix = format!("{} ", MASQUERADE_COMMENT);
        rules
            .lines()
            .filter(|l| l.ends_with(" -j RETURN"))
            .filter_map(|l| {
                let (_, rest) = l.split_once(" --comment \"")?;
                let (rule_comment, _) = rest.split_once('"')?;
                let root = rule_comment.strip_prefix(&prefix)?;
                let mut fields = l.split_whitespace();
                fields.find(|f| *f == "-s")?;
                let cidr = fields.next()?.parse::<Ipv4Network>().ok()?;
                let stale = if rule_comment == comment {
                    keep != Some(cidr)
                } else {
                    !root_exists(Path::new(root))
                };
                stale.then(|| (cidr, rule_comment.to_string()))
            })
            .collect()
    }
}

impl Drop for NatExemption {
    fn drop(&mut self) {
        debug!("Removing NAT exemption rule for CIDR {}", self.cidr);
        if !matches!(
            Self::iptables(&Self::args(self.cidr, &self.comment, "-D")),
            Ok((true, _))
        ) {
            debug!("Unable to remove the NAT exemption rule");
        }
    }
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use crate::config::tests::{test_config, test_config_wrong_cidr};
    use std::os::unix::ffi::OsStrExt;

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
        let c = crate::config::tests::parse(&["--nodes=3"])?;
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
    fn node_hostname_is_lowercase() -> Result<()> {
        assert_eq!(
            Network::node_hostname(OsStr::new("My-Host.Local"))?,
            "my-host.local"
        );
        assert_eq!(Network::node_hostname(OsStr::new("node-1"))?, "node-1");
        assert!(Network::node_hostname(OsStr::from_bytes(b"\xff")).is_err());
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
        let comment = NatExemption::comment(Path::new("/run/kx"));
        let args = NatExemption::args("10.10.0.0/17".parse()?, &comment, "-A");
        assert_eq!(
            args.join(" "),
            "-w -t nat -A POSTROUTING -s 10.10.0.0/17 -d 10.10.0.0/17 \
             -m comment --comment kubernix /run/kx -j RETURN"
        );
        Ok(())
    }

    const RULES: &str = concat!(
        "-P POSTROUTING ACCEPT\n",
        "-A POSTROUTING -m comment --comment \"kubernetes postrouting rules\" ",
        "-j KUBE-POSTROUTING\n",
        "-A POSTROUTING -s 10.10.0.0/17 -d 10.10.0.0/17 ",
        "-m comment --comment \"kubernix /run/kx\" -j RETURN\n",
        "-A POSTROUTING -s 10.20.0.0/17 -d 10.20.0.0/17 ",
        "-m comment --comment \"kubernix /run/kx\" -j RETURN\n",
        "-A POSTROUTING -s 10.30.0.0/17 -d 10.30.0.0/17 ",
        "-m comment --comment \"kubernix /run/other\" -j RETURN\n",
        "-A POSTROUTING -s 10.40.0.0/17 -d 10.40.0.0/17 ",
        "-m comment --comment \"kubernix /run/removed dir\" -j RETURN\n",
    );

    fn stale(keep: Option<&str>) -> Result<Vec<(String, String)>> {
        let keep = keep.map(str::parse).transpose()?;
        Ok(
            NatExemption::stale_rules(RULES, keep, "kubernix /run/kx", |root| {
                root == Path::new("/run/kx") || root == Path::new("/run/other")
            })
            .into_iter()
            .map(|(cidr, comment)| (cidr.to_string(), comment))
            .collect(),
        )
    }

    #[test]
    fn nat_exemption_stale_rules() -> Result<()> {
        // Rules of the same root with another CIDR and of removed roots
        assert_eq!(
            stale(Some("10.10.0.0/17"))?,
            [
                ("10.20.0.0/17".into(), "kubernix /run/kx".into()),
                ("10.40.0.0/17".into(), "kubernix /run/removed dir".into()),
            ]
        );
        // All rules of the same root if there is none to keep
        assert_eq!(
            stale(None)?,
            [
                ("10.10.0.0/17".into(), "kubernix /run/kx".into()),
                ("10.20.0.0/17".into(), "kubernix /run/kx".into()),
                ("10.40.0.0/17".into(), "kubernix /run/removed dir".into()),
            ]
        );
        Ok(())
    }

    #[test]
    fn parse_host_ips_skips_bridges() {
        let output = concat!(
            "2: enp0s31f6    inet 192.168.1.10/24 brd 192.168.1.255 scope global enp0s31f6\\\n",
            "3: kubernix.0    inet 10.10.0.1/19 brd 10.10.31.255 scope global kubernix.0\\\n",
            "4: tap0    inet 10.0.2.100/24 brd 10.0.2.255 scope global tap0\\\n",
            "5: kx0123abcd.1    inet 10.10.32.1/19 brd 10.10.63.255 scope global kx0123abcd.1\\\n",
        );
        assert_eq!(
            Network::parse_host_ips(output),
            [Ipv4Addr::new(192, 168, 1, 10), Ipv4Addr::new(10, 0, 2, 100)]
        );
    }

    #[test]
    fn bridge_names() -> Result<()> {
        let c = crate::config::tests::parse(&["--root=/a"])?;
        let n = Network::new(&c)?;
        assert_eq!(
            n.bridge(0),
            format!("kx{}.0", System::root_id(Path::new("/a")))
        );
        // IFNAMSIZ includes the terminating null byte
        assert!(n.bridge(254).len() < 16);
        for node in [0, 9, 254] {
            assert!(Network::is_bridge(&n.bridge(node)));
        }
        assert!(Network::is_bridge("kubernix.0"));
        for name in [
            "kx",
            "kx0123abcd",
            "kx0123abcd.",
            "kx0123abcg.0",
            "kxe40c292.0",
            "eth0",
        ] {
            assert!(!Network::is_bridge(name), "{}", name);
        }
        Ok(())
    }

    #[test]
    fn bridge_names_from_link_output() {
        let output = concat!(
            "5: kx0123abcd.0: <NO-CARRIER,BROADCAST,MULTICAST,UP> mtu 1500 qdisc noqueue\n",
            "6: kx0123abcd.12: <BROADCAST,MULTICAST,UP,LOWER_UP> mtu 1500 qdisc noqueue\n",
            "7: kx99999999.0: <BROADCAST,MULTICAST,UP,LOWER_UP> mtu 1500 qdisc noqueue\n",
            "8: kubernix.0: <BROADCAST,MULTICAST,UP,LOWER_UP> mtu 1500 qdisc noqueue\n",
        );
        assert_eq!(
            Bridges::names(output, "kx0123abcd."),
            ["kx0123abcd.0", "kx0123abcd.12"]
        );
    }

    #[test]
    fn node_ips_are_bridge_gateways_in_root_mode() -> Result<()> {
        let mut c = crate::config::tests::parse(&["--nodes=3"])?;
        let n = Network::new(&c)?;
        assert_eq!(n.gateway(1)?.to_string(), "10.10.32.1/19");
        assert_eq!(n.node_ip(2)?, Some(Ipv4Addr::new(10, 10, 64, 1)));
        assert_eq!(
            n.node_ips()?,
            [
                Ipv4Addr::new(10, 10, 0, 1),
                Ipv4Addr::new(10, 10, 32, 1),
                Ipv4Addr::new(10, 10, 64, 1)
            ]
        );
        assert!(n.gateway(3).is_err());

        c.set_rootless(true);
        let n = Network::new(&c)?;
        assert_eq!(n.node_ip(0)?, None);
        assert!(n.node_ips()?.is_empty());
        Ok(())
    }

    #[test]
    fn listen_addresses_per_node() -> Result<()> {
        let n = test_network()?;
        let addresses = n.listen_addresses(2);
        let ports = addresses.iter().map(|(a, _)| a.port()).collect::<Vec<_>>();
        assert_eq!(
            ports,
            [
                2379, 2380, 6443, 10257, 10259, 10249, 10256, 11250, 12250, 11251, 12251
            ]
        );
        assert_eq!(addresses[0].0, *n.etcd_client());
        assert_eq!(addresses[2].0.ip(), Ipv4Addr::UNSPECIFIED);
        Ok(())
    }

    #[test]
    fn port_in_use_describes_user() -> Result<()> {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))?;
        let address = listener.local_addr()?;
        let message = Network::port_in_use(address, "of etcd")?.context("expected in use")?;
        assert!(
            message.starts_with(&format!(
                "Port {} of etcd is already in use. ",
                address.port()
            )),
            "{}",
            message
        );
        assert!(message.contains("kubernix stop --root"), "{}", message);
        // Another test may bind the port once it is free again, so check
        // another loopback address while the listener still holds it: no
        // other socket can bind the port on all addresses meanwhile.
        let other = SocketAddr::from((Ipv4Addr::new(127, 0, 0, 2), address.port()));
        assert_eq!(Network::port_in_use(other, "of etcd")?, None);
        drop(listener);
        Ok(())
    }

    #[test]
    fn check_ports_detects_used_port() -> Result<()> {
        let n = test_network()?;
        let (address, _) = n.listen_addresses(1)[0].clone();
        // The port may be used by a cluster on the machine running the tests
        let Ok(_listener) = TcpListener::bind(address) else {
            return Ok(());
        };
        let err = n.check_ports(1).unwrap_err().to_string();
        assert!(
            err.contains("Port 2379 of etcd is already in use"),
            "{}",
            err
        );
        assert!(err.contains("kubernix status --root"), "{}", err);
        Ok(())
    }

    #[test]
    fn overlapping_routes_skip_own_bridges_and_host_bits() -> Result<()> {
        let output = concat!(
            "default via 192.168.1.1 dev enp0s31f6 proto dhcp metric 100\n",
            "10.10.0.0/17 dev kx0123abcd.0 proto kernel scope link src 10.10.0.1\n",
            "10.10.0.0/17 dev kubernix.0 proto kernel scope link src 10.10.0.1\n",
            "10.10.0.0/19 dev kx99999999.1 proto kernel scope link src 10.10.0.1\n",
            "10.10.200.0/24 dev wg0 scope link\n",
            "10.0.0.0/8 dev vpn0 scope link\n",
            "192.168.1.0/24 dev enp0s31f6 proto kernel scope link src 192.168.1.10\n",
        );
        // The CIDR of the configuration may have host bits
        let cidr: Ipv4Network = "10.10.1.1/16".parse()?;
        let cidr = Ipv4Network::new(cidr.network(), cidr.prefix())?;
        // The bridges of other roots and former versions are reported
        assert_eq!(
            strings(&Network::overlapping_routes(
                output,
                cidr,
                Some("kx0123abcd.")
            )),
            [
                "10.10.0.0/17",
                "10.10.0.0/19",
                "10.10.200.0/24",
                "10.0.0.0/8"
            ]
        );
        assert_eq!(
            strings(&Network::overlapping_routes(output, cidr, None)),
            [
                "10.10.0.0/17",
                "10.10.0.0/17",
                "10.10.0.0/19",
                "10.10.200.0/24",
                "10.0.0.0/8"
            ]
        );
        Ok(())
    }

    #[test]
    fn overlapping_bridges_of_other_roots() -> Result<()> {
        let n = Network::new(&crate::config::tests::parse(&["--root=/a"])?)?;
        let own = n.bridge(0);
        let output = format!(
            concat!(
                "2: enp0s31f6    inet 10.10.0.10/24 brd 10.10.0.255 scope global enp0s31f6\\\n",
                "3: kubernix.0    inet 10.10.0.1/17 brd 10.10.127.255 scope global kubernix.0\\\n",
                "4: {own}    inet 10.10.0.1/17 brd 10.10.127.255 scope global {own}\\\n",
                "5: kx99999999.0    inet 10.10.128.1/24 scope global kx99999999.0\\\n",
                "6: kx99999999.1    inet 10.20.0.1/17 scope global kx99999999.1\\\n",
                "7: kx88888888.0    inet 10.0.0.1/8 scope global kx88888888.0\\\n",
                "8: docker0    inet 10.10.64.1/24 scope global docker0\\\n",
            ),
            own = own,
        );
        // Only the kubernix bridges of other roots, also with an address in
        // the service CIDR or a larger network
        assert_eq!(
            n.overlapping_bridges(&output),
            ["kubernix.0", "kx99999999.0", "kx88888888.0"]
        );
        Ok(())
    }

    #[test]
    fn bridge_mac_is_stable_and_local() -> Result<()> {
        let n = Network::new(&crate::config::tests::parse(&["--root=/a", "--nodes=3"])?)?;
        let id = System::root_id(Path::new("/a"));
        let mac = n.bridge_mac(2);
        assert_eq!(
            mac,
            format!(
                "02:{}:{}:{}:{}:02",
                &id[0..2],
                &id[2..4],
                &id[4..6],
                &id[6..8]
            )
        );
        assert_ne!(n.bridge_mac(0), n.bridge_mac(1));
        let other = Network::new(&crate::config::tests::parse(&["--root=/b", "--nodes=3"])?)?;
        assert_ne!(n.bridge_mac(0), other.bridge_mac(0));
        Ok(())
    }

    #[test]
    fn kubelet_ips_and_advertise_address() -> Result<()> {
        let mut c = crate::config::tests::parse(&["--nodes=2"])?;
        let n = Network::new(&c)?;
        // The node IPs in root mode, independent of the host addresses
        assert_eq!(
            n.kubelet_ips()?,
            [Ipv4Addr::new(10, 10, 0, 1), Ipv4Addr::new(10, 10, 64, 1)]
        );
        assert_eq!(n.advertise_address()?, Some(Ipv4Addr::new(10, 10, 0, 1)));

        c.set_rootless(true);
        let n = Network::new(&c)?;
        assert_eq!(n.advertise_address()?, None);
        Ok(())
    }

    #[test]
    fn loopback_nameserver() {
        assert!(Network::has_loopback_nameserver(
            "# stub\nnameserver 127.0.0.53\noptions edns0 trust-ad\nsearch .\n"
        ));
        assert!(Network::has_loopback_nameserver("nameserver ::1\n"));
        assert!(!Network::has_loopback_nameserver(
            "nameserver 192.168.1.1\n# nameserver 127.0.0.53\n"
        ));
        assert!(!Network::has_loopback_nameserver(""));
    }
}
