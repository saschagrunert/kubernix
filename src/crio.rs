use crate::{
    Config, RUNTIME_ENV,
    container::Container,
    network::Network,
    node::Node,
    process::{Process, ProcessState, Stoppable},
    system::System,
};
use anyhow::{Context, Result, bail};
use log::debug;
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
            bail!("Socket path '{}' is too long", path.display())
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
        let node_name = Node::name(config, network, node);

        // In multi-node mode, CRI-O runs inside a container where host paths
        // don't exist. Use empty paths so CRI-O resolves binaries from $PATH
        // (set up by nix-shell in the container).
        let conmon: String;
        let crun_path: String;
        let plugin_dir: String;
        if config.multi_node() {
            // Empty paths let CRI-O resolve conmon/crun from $PATH inside the container.
            conmon = String::new();
            crun_path = String::new();
            // Placeholder: overridden by --cni-plugin-dir CLI arg at startup.
            plugin_dir = "/tmp/cni-plugins".to_string();
        } else {
            conmon = System::find_executable("conmon")?.display().to_string();
            crun_path = System::find_executable("crun")?.display().to_string();
            let loopback = System::find_executable("loopback")?;
            plugin_dir = loopback
                .parent()
                .context("Unable to find CNI plugin dir")?
                .display()
                .to_string();
        };

        let dir = Self::path(config, network, node);
        let config_dir = dir.join("crio.conf.d");
        let config_file = config_dir.join("crio.conf");
        let network_dir = dir.join("cni");
        let socket = Self::socket(config, network, node)?;

        if !dir.exists() {
            create_dir_all(&dir)?;
            create_dir_all(&network_dir)?;
            create_dir_all(&config_dir)?;

            let containers_dir = dir.join("containers");
            fs::write(
                &config_file,
                format!(
                    include_str!("assets/crio.conf"),
                    conmon = conmon,
                    containers_root = containers_dir.join("storage").display(),
                    containers_runroot = containers_dir.join("run").display(),
                    listen = socket,
                    log_dir = dir.join("log").display(),
                    network_dir = network_dir.display(),
                    plugin_dir = plugin_dir,
                    exits_dir = dir.join("exits").display(),
                    runtime_path = crun_path,
                    runtime_root = dir.join("crun").display(),
                    signature_policy = Container::policy_json(config).display(),
                    storage_driver = if config.multi_node() || System::in_container()? {
                        "vfs"
                    } else {
                        "overlay"
                    },
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
        let config_dir_arg = format!("--config-dir={}", config_dir.display());
        let args: &[&str] = &[&config_dir_arg];

        let mut process = if config.multi_node() {
            // Run inside a container, resolve CNI plugin dir from $PATH at runtime
            let identifier = format!("CRI-O {}", node_name);
            let plugin_dir_arg =
                r#"--cni-plugin-dir=$(dirname $(which loopback || echo loopback_not_found))"#
                    .to_string();
            let container_args: &[&str] = &[&config_dir_arg, &plugin_dir_arg];
            Container::start(config, &dir, &identifier, CRIO, &node_name, container_args)?
        } else {
            // Run as usual process
            Process::start(&dir, "CRI-O", CRIO, args)?
        };
        process.wait_ready("No systemd watchdog enabled")?;

        Ok(Box::new(Self {
            process,
            socket,
            node_name,
        }))
    }

    /// Retrieve the CRI socket
    pub fn socket(config: &Config, network: &Network, node: u8) -> Result<CriSocket> {
        CriSocket::new(Self::path(config, network, node).join("crio.sock"))
    }

    /// Retrieve the working path for the node
    fn path(config: &Config, network: &Network, node: u8) -> PathBuf {
        config
            .root()
            .join(CRIO)
            .join(Node::name(config, network, node))
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
            debug!("crictl pods stdout ({}): {}", self.node_name, stdout);
            debug!(
                "crictl pods stderr ({}): {}",
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
                debug!("crictl rmp ({}): {:?}", self.node_name, output);
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
