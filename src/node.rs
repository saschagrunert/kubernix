//! Node naming utilities.
//!
//! Provides consistent node name generation used across kubelet,
//! CRI-O, and container components for both single-node and multi-node
//! configurations.

use crate::{Config, network::Network};

/// Generates node names based on cluster configuration.
pub struct Node;

impl Node {
    /// Retrieve the node name for the node number
    pub fn name(config: &Config, network: &Network, number: u8) -> String {
        if config.multi_node() {
            Self::raw(number)
        } else {
            network.hostname().into()
        }
    }

    /// Retrieve the raw node name
    pub fn raw(number: u8) -> String {
        const PREFIX: &str = "node";
        format!("{}-{}", PREFIX, number)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{config::tests::test_config, network::tests::test_network};
    use anyhow::Result;

    #[test]
    fn raw_name() {
        assert_eq!(Node::raw(0), "node-0");
        assert_eq!(Node::raw(5), "node-5");
    }

    #[test]
    fn single_node_uses_hostname() -> Result<()> {
        let c = test_config()?;
        let n = test_network()?;
        assert_eq!(Node::name(&c, &n, 0), *n.hostname());
        Ok(())
    }
}
