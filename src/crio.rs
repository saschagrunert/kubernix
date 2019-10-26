use crate::{
    container::Container,
    network::Network,
    node::Node,
    process::{Process, ProcessState, Stoppable},
    system::System,
    Config, RUNTIME_ENV,
};
use anyhow::{bail, Context, Result};
use log::{debug, info};
use serde_json::{json, to_string_pretty};
use std::{
    fmt::{self, Display, Formatter},
    fs::{self, create_dir_all},
    path::PathBuf,
    process::Command,
};

pub struct Crio {
    process: Process,
    socket: CriSocket,
    node_name: String,
}

/// Simple CRI socket abstraction
pub struct CriSocket(PathBuf);

impl Display for CriSocket {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0.display())
    }
}

impl CriSocket {
    pub fn new(path: PathBuf) -> Result<CriSocket> {
        if path.display().to_string().len() > 100 {
            bail!("Socket path '{}' is too long")
        }
        Ok(CriSocket(path))
    }

    pub fn to_socket_string(&self) -> String {
        format!("unix://{}", self.0.display())
    }
}

const CRIO: &str = "crio";

impl Crio {
    pub fn start(config: &Config, node: u8, network: &Network) -> ProcessState {
        let node_name = Node::name(node);
        info!("Starting CRI-O ({})", node_name);

        let conmon = System::find_executable("conmon")?;
        let loopback = System::find_executable("loopback")?;
        let cni_plugin = loopback.parent().context("Unable to find CNI plugin dir")?;

        let dir = Self::path(config, node);
        let config_file = dir.join("crio.conf");
        let network_dir = dir.join("cni");
        let socket = Self::socket(config, node)?;

        if !dir.exists() {
            create_dir_all(&dir)?;
            create_dir_all(&network_dir)?;

            let containers_dir = dir.join("containers");
            fs::write(
                &config_file,
                format!(
                    include_str!("assets/crio.conf"),
                    conmon = conmon.display(),
                    containers_root = containers_dir.join("storage").display(),
                    containers_runroot = containers_dir.join("run").display(),
                    listen = socket,
                    log_dir = dir.join("log").display(),
                    network_dir = network_dir.display(),
                    plugin_dir = cni_plugin.display(),
                    exits_dir = dir.join("exits").display(),
                    runtime_path = System::find_executable("runc")?.display(),
                    runtime_root = dir.join("runc").display(),
                    signature_policy = Container::policy_json(config).display(),
                    version_file = dir.join("version").display(),
                ),
            )?;

            let cidr = network
                .crio_cidrs()
                .get(node as usize)
                .with_context(|| format!("Unable to find CIDR for {}", node_name))?;
            fs::write(
                network_dir.join("10-bridge.json"),
                to_string_pretty(&json!({
                    "cniVersion": "0.3.1",
                    "name": format!("kubernix-{}", node_name),
                    "type": "bridge",
                    "bridge": format!("{}.{}", Network::INTERFACE_PREFIX, node),
                    "isGateway": true,
                    "ipMasq": true,
                    "hairpinMode": true,
                    "ipam": {
                        "type": "host-local",
                        "routes": [{ "dst": "0.0.0.0/0" }],
                        "ranges": [[{ "subnet": cidr }]]
                    }
                }))?,
            )?;
        }
        let args: &[&str] = &[&format!("--config={}", config_file.display())];

        // Run inside a container
        let identifier = format!("CRI-O {}", node_name);
        let mut process = Container::start(config, &dir, &identifier, CRIO, &node_name, args)?;
        process.wait_ready("sandboxes:")?;

        info!("CRI-O is ready ({})", node_name);
        Ok(Box::new(Self {
            process,
            socket,
            node_name,
        }))
    }

    /// Retrieve the CRI socket
    pub fn socket(config: &Config, node: u8) -> Result<CriSocket> {
        CriSocket::new(Self::path(config, node).join("crio.sock"))
    }

    /// Retrieve the working path for the node
    fn path(config: &Config, node: u8) -> PathBuf {
        config.root().join(CRIO).join(Node::name(node))
    }

    /// Remove all containers via crictl invocations
    fn remove_all_containers(&self) -> Result<()> {
        debug!("Removing all CRI-O workloads on {}", self.node_name);

        let output = Command::new("crictl")
            .env(RUNTIME_ENV, self.socket.to_socket_string())
            .arg("pods")
            .arg("-q")
            .output()?;
        let stdout = String::from_utf8(output.stdout)?;
        if !output.status.success() {
            debug!("critcl pods stdout ({}): {}", self.node_name, stdout);
            debug!(
                "critcl pods stderr ({}): {}",
                self.node_name,
                String::from_utf8(output.stderr)?
            );
            bail!("crictl pods command failed ({})", self.node_name);
        }

        for x in stdout.lines() {
            debug!("Removing pod {} on {}", x, self.node_name);
            let output = Command::new("crictl")
                .env(RUNTIME_ENV, self.socket.to_socket_string())
                .arg("rmp")
                .arg("-f")
                .arg(x)
                .output()?;
            if !output.status.success() {
                debug!("critcl rmp ({}): {:?}", self.node_name, output);
                bail!("crictl rmp command failed ({})", self.node_name);
            }
        }

        debug!("All workloads removed on {}", self.node_name);
        Ok(())
    }
}

impl Stoppable for Crio {
    fn stop(&mut self) -> Result<()> {
        // Remove all running containers
        self.remove_all_containers()
            .with_context(|| format!("Unable to remove CRI-O containers on {}", self.node_name,))?;

        // Stop the process, should never really fail
        self.process.stop()
    }
}

#[cfg(test)]
pub mod tests {
    use super::*;

    #[test]
    fn cri_socket_success() -> Result<()> {
        CriSocket::new("/some/path.sock".into())?;
        Ok(())
    }

    #[test]
    fn cri_socket_failure() {
        assert!(CriSocket::new("a".repeat(101).into()).is_err());
    }
}
