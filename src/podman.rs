//! Podman container runtime integration.
//!
//! Provides podman-specific CLI arguments for image builds and container
//! execution, including conmon, crun, and storage driver configuration.

use crate::{Config, system::System};
use anyhow::Result;
use log::LevelFilter;
use std::path::Path;

/// Podman-specific argument builder for container operations.
pub struct Podman;

impl Podman {
    /// The executable name
    pub const EXECUTABLE: &'static str = "podman";

    /// Returns true if podman is configured as container runtime
    pub fn is_configured(config: &Config) -> bool {
        config.container_runtime() == Self::EXECUTABLE
    }

    /// Retrieve the podman args for a command which verifies image
    /// signatures, like `build` or `import`.
    pub fn signature_args(
        config: &Config,
        command: &str,
        policy_json: &Path,
    ) -> Result<Vec<String>> {
        let mut args = Self::default_args(config)?;
        args.extend(vec![
            command.into(),
            format!("--signature-policy={}", policy_json.display()),
        ]);

        Ok(args)
    }

    /// Podman args which should apply to every command
    pub fn default_args(config: &Config) -> Result<Vec<String>> {
        let log_level = if config.log_level() >= LevelFilter::Debug {
            "debug".into()
        } else {
            config.log_level().to_string()
        };
        let mut args = vec![
            format!("--conmon={}", System::find_executable("conmon")?.display()),
            format!("--log-level={}", log_level),
            format!("--runtime={}", System::find_executable("crun")?.display()),
            "--cgroup-manager=cgroupfs".into(),
            "--events-backend=none".into(),
        ];
        if System::in_container()? {
            args.push("--storage-driver=vfs".into());
        }
        Ok(args)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::tests::test_config;
    use std::path::PathBuf;

    #[test]
    fn is_configured_success() -> Result<()> {
        let c = test_config()?;
        assert!(Podman::is_configured(&c));
        Ok(())
    }

    #[test]
    #[ignore = "requires conmon in $PATH"]
    fn signature_args_success() -> Result<()> {
        let c = test_config()?;
        let p = PathBuf::from("policy.json");
        Podman::signature_args(&c, "build", &p)?;
        Ok(())
    }

    #[test]
    #[ignore = "requires conmon in $PATH"]
    fn default_args_success() -> Result<()> {
        let c = test_config()?;
        assert!(!Podman::default_args(&c)?.is_empty());
        Ok(())
    }
}
