//! Shared CRI (Container Runtime Interface) abstractions.
//!
//! Contains types and helpers used by both CRI-O and containerd
//! implementations.

use crate::{
    config::{Config, CriRuntime},
    containerd::Containerd,
    crio::Crio,
    network::Network,
};
use anyhow::{Context, Result, bail};
use log::debug;
use serde_json::{json, to_string_pretty};
use std::{
    fmt::{self, Display, Formatter},
    fs,
    path::{Path, PathBuf},
    process::Command,
};

/// Environment variable name for the CRI socket endpoint.
pub const RUNTIME_ENV: &str = "CONTAINER_RUNTIME_ENDPOINT";

/// Maximum usable bytes in a Unix socket path (108 - 1 for null terminator).
pub const MAX_SOCKET_PATH_LEN: usize = 107;

/// Simple CRI socket abstraction.
pub struct CriSocket(PathBuf);

impl Display for CriSocket {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0.display())
    }
}

impl CriSocket {
    /// Create a new CRI socket, validating the path length against Unix limits.
    pub fn new(path: PathBuf) -> Result<CriSocket> {
        if path.display().to_string().len() > MAX_SOCKET_PATH_LEN {
            bail!("Socket path '{}' is too long", path.display())
        }
        Ok(CriSocket(path))
    }

    /// Format the socket path as a `unix://` URI for CRI clients.
    pub fn to_socket_string(&self) -> String {
        format!("unix://{}", self.0.display())
    }
}

/// Return the CRI socket for the configured runtime and node.
pub fn cri_socket(config: &Config, network: &Network, node: u8) -> Result<CriSocket> {
    match config.cri_runtime() {
        CriRuntime::Crio => Crio::socket(config, network, node),
        CriRuntime::Containerd => Containerd::socket(config, network, node),
    }
}

/// Write the CNI bridge network configuration for a node.
pub fn write_cni_config(
    cni_conf_dir: &Path,
    node_name: &str,
    node: u8,
    network: &Network,
) -> Result<()> {
    let cidr = network
        .pod_cidrs()
        .get(node as usize)
        .with_context(|| format!("Unable to find CIDR for {}", node_name))?;
    fs::write(
        cni_conf_dir.join("10-bridge.json"),
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
    Ok(())
}

/// Remove all containers and pods via crictl.
pub fn remove_all_containers(label: &str, socket: &CriSocket, node_name: &str) -> Result<()> {
    debug!("Removing all {} workloads on {}", label, node_name);

    let output = Command::new("crictl")
        .env(RUNTIME_ENV, socket.to_socket_string())
        .arg("pods")
        .arg("-q")
        .output()?;
    let stdout = String::from_utf8(output.stdout)?;
    if !output.status.success() {
        debug!("crictl pods stdout ({}): {}", node_name, stdout);
        debug!(
            "crictl pods stderr ({}): {}",
            node_name,
            String::from_utf8(output.stderr)?
        );
        bail!("crictl pods command failed ({})", node_name);
    }

    for x in stdout.lines() {
        debug!("Removing pod {} on {}", x, node_name);
        let output = Command::new("crictl")
            .env(RUNTIME_ENV, socket.to_socket_string())
            .arg("rmp")
            .arg("-f")
            .arg(x)
            .output()?;
        if !output.status.success() {
            debug!("crictl rmp ({}): {:?}", node_name, output);
            bail!("crictl rmp command failed ({})", node_name);
        }
    }

    debug!("All workloads removed on {}", node_name);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cri_socket_success() -> Result<()> {
        CriSocket::new("/some/path.sock".into())?;
        Ok(())
    }

    #[test]
    fn cri_socket_failure() {
        assert!(CriSocket::new("a".repeat(MAX_SOCKET_PATH_LEN + 1).into()).is_err());
    }

    #[test]
    fn cri_socket_string_format() -> Result<()> {
        let socket = CriSocket::new("/run/crio.sock".into())?;
        assert_eq!(socket.to_socket_string(), "unix:///run/crio.sock");
        assert_eq!(socket.to_string(), "/run/crio.sock");
        Ok(())
    }
}
