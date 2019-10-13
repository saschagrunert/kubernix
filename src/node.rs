use crate::{
    crio::Crio,
    kubeconfig::KubeConfig,
    kubelet::Kubelet,
    network::Network,
    pki::Pki,
    process::{ProcessState, Started, Stoppable},
    Config,
};
use failure::Fallible;
use log::{error, info};
use std::process::{Command, Stdio};

pub struct Node {
    crio: Started,
    kubelet: Started,
    name: String,
    runtime: String,
}

impl Node {
    /// Start a node in a container base image
    pub fn start(
        config: &Config,
        node: u8,
        network: &Network,
        pki: &Pki,
        kubeconfig: &KubeConfig,
    ) -> ProcessState {
        let name = Node::name(node);
        info!("Starting {}", name);

        let crio = Crio::start(config, node, network)?;
        let kubelet = Kubelet::start(config, node, network, pki, kubeconfig)?;

        info!("{} is ready", name);
        Ok(Box::new(Self {
            crio,
            kubelet,
            name,
            runtime: config.container_runtime().to_owned(),
        }))
    }

    /// Retrieve the node name for the node number
    pub fn name(number: u8) -> String {
        const PREFIX: &str = "node";
        format!("{}-{}", PREFIX, number)
    }
}

impl Stoppable for Node {
    fn stop(&mut self) -> Fallible<()> {
        // Stop the processes
        if let Err(e) = self.kubelet.stop() {
            error!("Unable to stop kubelet on {}: {}", self.name, e)
        }
        self.crio.stop()?;

        // Remove possible dead containers
        Command::new(&self.runtime)
            .arg("rm")
            .arg("-f")
            .stderr(Stdio::null())
            .stdout(Stdio::null())
            .arg(&self.name)
            .status()?;

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn name_success() {
        assert_eq!(Node::name(10), "node-10")
    }
}
