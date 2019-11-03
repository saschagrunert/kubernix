use crate::{network::Network, Config};

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
