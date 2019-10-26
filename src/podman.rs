use crate::{network::Network, system::System, Config};
use anyhow::{Context, Result};
use log::LevelFilter;
use serde_json::{json, to_string_pretty};
use std::{
    fs::{self, create_dir_all},
    path::{Path, PathBuf},
};

pub struct Podman;

impl Podman {
    pub const EXECUTABLE: &'static str = "podman";

    /// Returns true if podman is configured as container runtime
    pub fn is_configured(config: &Config) -> bool {
        config.container_runtime() == Self::EXECUTABLE
    }

    /// Retrieve the podman build args
    pub fn build_args(
        config: &Config,
        network: &Network,
        policy_json: &Path,
    ) -> Result<Vec<String>> {
        // Prepare the CNI dir
        let dir = Self::cni_dir(config);
        create_dir_all(&dir)?;
        fs::write(
            &dir.join("87-podman-bridge.conflist"),
            to_string_pretty(&json!({
                "cniVersion": "0.4.0",
                "name": "podman",
                "plugins": [{
                    "type": "bridge",
                    "bridge": "cni-podman0",
                    "isGateway": true,
                    "ipMasq": true,
                    "ipam": {
                        "type": "host-local",
                        "routes": [{ "dst": "0.0.0.0/0" }],
                        "ranges": [[{
                            "subnet": network.podman_cidr(),
                            "gateway": network
                                .podman_cidr()
                                .nth(1)
                                .context("Unable to retrieve gateway IP from config CIDR")?,
                        }]]
                    }
                },
                    { "type": "portmap", "capabilities": { "portMappings": true } },
                    { "type": "firewall", "backend": "iptables" }
                ]
            }))?,
        )?;

        let mut args = Self::default_args(config)?;
        args.extend(vec![
            "build".into(),
            format!("--signature-policy={}", policy_json.display()),
        ]);

        Ok(args)
    }

    /// Podman args which should apply to every command
    pub fn default_args(config: &Config) -> Result<Vec<String>> {
        let log_level = if config.log_level() >= LevelFilter::Debug {
            "DEBUG".into()
        } else {
            config.log_level().to_string()
        };
        Ok(vec![
            format!("--log-level={}", log_level),
            format!(
                "--storage-driver={}",
                if System::in_container()? { "vfs" } else { "" }
            ),
            format!("--cni-config-dir={}", Self::cni_dir(config).display()),
            "--events-backend=none".into(),
            "--cgroup-manager=cgroupfs".into(),
        ])
    }

    /// Retrieve the internal CNI directory
    fn cni_dir(config: &Config) -> PathBuf {
        config.root().join("podman")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        config::tests::{test_config, test_config_wrong_root},
        network::tests::test_network,
    };

    #[test]
    fn is_configured_success() -> Result<()> {
        let c = test_config()?;
        assert!(Podman::is_configured(&c));
        Ok(())
    }

    #[test]
    fn build_args_success() -> Result<()> {
        let c = test_config()?;
        let p = PathBuf::from("policy.json");
        let n = test_network()?;
        Podman::build_args(&c, &n, &p)?;
        Ok(())
    }

    #[test]
    fn build_args_failure() -> Result<()> {
        let c = test_config_wrong_root()?;
        let p = PathBuf::from("policy.json");
        let n = test_network()?;
        assert!(Podman::build_args(&c, &n, &p).is_err());
        Ok(())
    }

    #[test]
    fn default_args_success() -> Result<()> {
        let c = test_config()?;
        assert!(!Podman::default_args(&c)?.is_empty());
        Ok(())
    }
}
