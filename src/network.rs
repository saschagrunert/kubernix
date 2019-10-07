use crate::Config;
use failure::{bail, format_err, Fallible};
use getset::Getters;
use ipnetwork::Ipv4Network;
use log::{debug, warn};
use std::{net::Ipv4Addr, process::Command};

#[derive(Getters)]
pub struct Network {
    #[get = "pub"]
    crio: Ipv4Network,

    #[get = "pub"]
    cluster: Ipv4Network,

    #[get = "pub"]
    service: Ipv4Network,
}

impl Network {
    /// The global name for the bridged interface
    pub const BRIDGE: &'static str = "kubernix1";

    /// Create a new network from the provided config
    pub fn new(config: &Config) -> Fallible<Self> {
        if config.cidr().prefix() > 24 {
            bail!(
                "Specified IP network {} is too small, please use at least a /24 subnet",
                config.cidr()
            )
        }

        Self::warn_overlapping_route(*config.cidr())?;

        let crio = Ipv4Network::new(config.cidr().ip(), config.cidr().prefix() + 1)?;
        debug!("Using crio CIDR {}", crio);

        let cluster = Ipv4Network::new(
            config
                .cidr()
                .nth(config.cidr().size() / 2)
                .ok_or_else(|| format_err!("Unable to retrieve cluster CIDR start IP"))?,
            config.cidr().prefix() + 2,
        )?;
        debug!("Using cluster CIDR {}", cluster);

        let service = Ipv4Network::new(
            config
                .cidr()
                .nth(config.cidr().size() / 2 + cluster.size())
                .ok_or_else(|| format_err!("Unable to retrieve service CIDR start IP"))?,
            config.cidr().prefix() + 3,
        )?;
        debug!("Using service CIDR {}", service);

        Ok(Self {
            crio,
            cluster,
            service,
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
            .filter(|x| !x.contains(Self::BRIDGE))
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
        self.service().nth(1).ok_or_else(|| {
            format_err!(
                "Unable to retrieve first IP from service CIDR: {}",
                self.service()
            )
        })
    }

    /// Retrieve the DNS address from the service CIDR
    pub fn dns(&self) -> Fallible<Ipv4Addr> {
        self.service().nth(2).ok_or_else(|| {
            format_err!(
                "Unable to retrieve second IP from service CIDR: {}",
                self.service()
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
        assert_eq!(n.dns()?, Ipv4Addr::new(10, 10, 192, 2));
        Ok(())
    }
}
