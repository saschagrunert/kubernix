//! Podman container runtime integration.
//!
//! Provides podman-specific CLI arguments for image builds and container
//! execution, including the OCI runtime and storage driver configuration.
//! Podman finds conmon in its own helper binaries directory.

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
        Ok(Self::args(
            config.log_level(),
            &System::find_executable("crun")?,
            System::in_container()?,
        ))
    }

    /// Podman args for the log level, the crun executable and whether
    /// kubernix runs inside a container.
    fn args(log_level: LevelFilter, crun: &Path, in_container: bool) -> Vec<String> {
        let log_level = if log_level >= LevelFilter::Debug {
            "debug".into()
        } else {
            log_level.to_string()
        };
        let mut args = vec![
            format!("--log-level={}", log_level),
            format!("--runtime={}", crun.display()),
            "--cgroup-manager=cgroupfs".into(),
            "--events-backend=none".into(),
        ];
        if in_container {
            args.push("--storage-driver=vfs".into());
        }
        args
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
    fn args_success() {
        let crun = PathBuf::from("/nix/bin/crun");
        assert_eq!(
            Podman::args(LevelFilter::Info, &crun, false),
            [
                "--log-level=INFO",
                "--runtime=/nix/bin/crun",
                "--cgroup-manager=cgroupfs",
                "--events-backend=none",
            ]
        );
        let args = Podman::args(LevelFilter::Trace, &crun, true);
        assert_eq!(args[0], "--log-level=debug");
        assert_eq!(
            args.last().map(String::as_str),
            Some("--storage-driver=vfs")
        );
        assert!(!args.iter().any(|a| a.starts_with("--conmon")));
    }
}
