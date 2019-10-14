use crate::{network::Network, Config};

pub struct Node;

impl Node {
    /// Retrieve the node name for the node number
    pub fn name(config: &Config, network: &Network, number: u8) -> String {
        if config.nodes() == 1 {
            network.hostname().to_owned()
        } else {
            const PREFIX: &str = "node";
            format!("{}-{}", PREFIX, number)
        }
    }
}
