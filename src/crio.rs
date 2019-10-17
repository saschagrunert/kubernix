use crate::{
    container::Container,
    network::Network,
    node::Node,
    process::{Process, ProcessState, Stoppable},
    system::System,
    Config, RUNTIME_ENV,
};
use failure::{bail, format_err, Fallible};
use log::{debug, info};
use serde_json::{json, to_string_pretty};
use std::{
    fmt::{Display, Formatter, Result},
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
    fn fmt(&self, f: &mut Formatter<'_>) -> Result {
        write!(f, "{}", self.0.display())
    }
}

impl CriSocket {
    pub fn new(path: PathBuf) -> Fallible<CriSocket> {
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
        let node_name = Node::name(config, network, node);
        info!("Starting CRI-O ({})", node_name);

        let conmon = System::find_executable("conmon")?;
        let loopback = System::find_executable("loopback")?;
        let cni_plugin = loopback
            .parent()
            .ok_or_else(|| format_err!("Unable to find CNI plugin dir"))?;

        let dir = Self::path(config, network, node);
        let crio_config = dir.join("crio.conf");
        let cni = dir.join("cni");

        if !dir.exists() {
            create_dir_all(&dir)?;
            create_dir_all(&cni)?;

            let cidr = network
                .crio_cidrs()
                .get(node as usize)
                .ok_or_else(|| format_err!("Unable to find CIDR for {}", node_name))?;
            fs::write(
                cni.join("10-bridge.json"),
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

            // Pseudo config to not load local configuration values
            fs::write(&crio_config, "")?;
        }
        let socket = Self::socket(config, network, node)?;

        let args = &[
            "--log-level=debug",
            "--registry=docker.io",
            &format!("--config={}", crio_config.display()),
            &format!("--conmon={}", conmon.display()),
            &format!("--listen={}", socket),
            &format!("--root={}", dir.join("storage").display()),
            &format!("--runroot={}", dir.join("run").display()),
            &format!("--cni-config-dir={}", cni.display()),
            &format!("--cni-plugin-dir={}", cni_plugin.display()),
            &format!(
                "--signature-policy={}",
                Container::policy_json(config).display()
            ),
            &format!(
                "--runtimes=local-runc:{}:{}",
                System::find_executable("runc")?.display(),
                dir.join("runc").display()
            ),
            "--default-runtime=local-runc",
            &format!(
                "--storage-driver={}",
                if config.nodes() > 1 || config.container() {
                    "vfs"
                } else {
                    "overlay"
                }
            ),
        ];

        let mut process = if config.nodes() > 1 {
            // Run inside a container
            Container::start(config, &dir, "CRI-O", CRIO, &node_name, args)?
        } else {
            // Run as usual process
            Process::start(&dir, "CRI-O", CRIO, args)?
        };
        process.wait_ready("sandboxes:")?;

        info!("CRI-O is ready ({})", node_name);
        Ok(Box::new(Self {
            process,
            socket,
            node_name,
        }))
    }

    /// Retrieve the CRI socket
    pub fn socket(config: &Config, network: &Network, node: u8) -> Fallible<CriSocket> {
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
    fn remove_all_containers(&self) -> Fallible<()> {
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
                debug!(
                    "critcl rmp stdout ({}): {}",
                    self.node_name,
                    String::from_utf8(output.stdout)?
                );
                debug!(
                    "critcl rmp stderr ({}): {}",
                    self.node_name,
                    String::from_utf8(output.stderr)?
                );
                bail!("crictl rmp command failed ({})", self.node_name);
            }
        }

        debug!("All workloads removed on {}", self.node_name);
        Ok(())
    }
}

impl Stoppable for Crio {
    fn stop(&mut self) -> Fallible<()> {
        // Remove all running containers
        self.remove_all_containers().map_err(|e| {
            format_err!(
                "Unable to remove CRI-O containers on {}: {}",
                self.node_name,
                e
            )
        })?;

        // Stop the process, should never really fail
        self.process.stop()
    }
}

#[cfg(test)]
pub mod tests {
    use super::*;

    #[test]
    fn cri_socket_success() -> Fallible<()> {
        CriSocket::new("/some/path.sock".into())?;
        Ok(())
    }

    #[test]
    fn cri_socket_failure() {
        assert!(CriSocket::new("a".repeat(101).into()).is_err());
    }
}
