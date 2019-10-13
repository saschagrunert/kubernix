use crate::Config;
use failure::{bail, format_err, Fallible};
use getset::Getters;
use ipnetwork::Ipv4Network;
use log::{debug, warn};
use std::{
    net::{Ipv4Addr, SocketAddr},
    process::Command,
};

#[derive(Getters)]
pub struct Network {
    #[get = "pub"]
    cluster_cidr: Ipv4Network,

    #[get = "pub"]
    crio_cidrs: Vec<Ipv4Network>,

    #[get = "pub"]
    service_cidr: Ipv4Network,

    #[get = "pub"]
    etcd_client: SocketAddr,

    #[get = "pub"]
    etcd_peer: SocketAddr,
}

impl Network {
    /// The global name for the interface
    pub const INTERFACE_PREFIX: &'static str = "kubernix";

    /// Create a new network from the provided config
    pub fn new(config: &Config) -> Fallible<Self> {
        // Preflight checks
        if config.cidr().prefix() > 24 {
            bail!(
                "Specified IP network {} is too small, please use at least a /24 subnet",
                config.cidr()
            )
        }
        Self::warn_overlapping_route(config.cidr())?;

        // Calculate the CIDRs
        let cluster_cidr = Ipv4Network::new(config.cidr().ip(), 24)?;
        debug!("Using cluster CIDR {}", cluster_cidr);

        let service_cidr = Ipv4Network::new(
            config
                .cidr()
                .nth(cluster_cidr.size())
                .ok_or_else(|| format_err!("Unable to retrieve service CIDR start IP"))?,
            24,
        )?;
        debug!("Using service CIDR {}", service_cidr);

        let mut crio_cidrs = vec![];
        let mut offset = cluster_cidr.size() + service_cidr.size();
        for node in 0..config.nodes() {
            let cidr = Ipv4Network::new(
                config
                    .cidr()
                    .nth(offset)
                    .ok_or_else(|| format_err!("Unable to retrieve CRI-O CIDR start IP"))?,
                24,
            )?;
            offset += cidr.size();
            debug!("Using CRI-O ({}) CIDR {}", node, cidr);
            crio_cidrs.push(cidr);
        }

        // Set the rest of the networking related adresses and paths
        let etcd_client = SocketAddr::new(Ipv4Addr::LOCALHOST.into(), 2379);
        let etcd_peer = SocketAddr::new(Ipv4Addr::LOCALHOST.into(), 2380);

        Ok(Self {
            cluster_cidr,
            crio_cidrs,
            service_cidr,
            etcd_client,
            etcd_peer,
        })
    }

    /// Check if there are overlapping routes and warn
    fn warn_overlapping_route(cidr: Ipv4Network) -> Fallible<()> {
        let cmd = Command::new("ip").arg("route").output()?;
        if !cmd.status.success() {
            bail!("Unable to obtain `ip` routes")
        }
        String::from_utf8(cmd.stdout)?
            .lines()
            .filter(|x| !x.contains(Self::INTERFACE_PREFIX))
            .filter_map(|x| x.split_whitespace().nth(0))
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

    /// Retrieve the DNS address from the service CIDR
    pub fn api(&self) -> Fallible<Ipv4Addr> {
        self.service_cidr().nth(1).ok_or_else(|| {
            format_err!(
                "Unable to retrieve first IP from service CIDR: {}",
                self.service_cidr()
            )
        })
    }

    /// Retrieve the DNS address from the service CIDR
    pub fn dns(&self) -> Fallible<Ipv4Addr> {
        self.service_cidr().nth(2).ok_or_else(|| {
            format_err!(
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

    pub fn test_network() -> Fallible<Network> {
        let c = test_config()?;
        Network::new(&c)
    }

    #[test]
    fn new_success() -> Fallible<()> {
        let c = test_config()?;
        Network::new(&c)?;
        Ok(())
    }

    #[test]
    fn new_failure() -> Fallible<()> {
        let c = test_config_wrong_cidr()?;
        assert!(Network::new(&c).is_err());
        Ok(())
    }

    #[test]
    fn dns_success() -> Fallible<()> {
        let c = test_config()?;
        let n = Network::new(&c)?;
        assert_eq!(n.dns()?, Ipv4Addr::new(10, 10, 1, 2));
        Ok(())
    }
}
