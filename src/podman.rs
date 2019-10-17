use crate::Config;
use failure::Fallible;
use log::LevelFilter;
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
    pub fn build_args(config: &Config, policy_json: &Path) -> Fallible<Vec<String>> {
        // Prepare the CNI dir
        let dir = Self::cni_dir(config);
        create_dir_all(&dir)?;
        fs::write(
            &dir.join("87-podman-bridge.conflist"),
            include_str!("assets/podman-bridge.json"),
        )?;

        let mut args = Self::default_args(config);
        args.extend(vec![
            "build".into(),
            format!("--signature-policy={}", policy_json.display()),
        ]);

        Ok(args)
    }

    /// Podman args which should apply to every command
    pub fn default_args(config: &Config) -> Vec<String> {
        let log_level = if config.log_level() >= LevelFilter::Debug {
            "DEBUG".into()
        } else {
            config.log_level().to_string()
        };
        vec![
            format!("--log-level={}", log_level),
            format!(
                "--storage-driver={}",
                if config.container() { "vfs" } else { "" }
            ),
            format!("--cni-config-dir={}", Self::cni_dir(config).display()),
            "--events-backend=none".into(),
            "--cgroup-manager=cgroupfs".into(),
        ]
    }

    fn cni_dir(config: &Config) -> PathBuf {
        config.root().join("podman")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::tests::{test_config, test_config_wrong_root};

    #[test]
    fn is_configured_success() -> Fallible<()> {
        let c = test_config()?;
        assert!(Podman::is_configured(&c));
        Ok(())
    }

    #[test]
    fn build_args_success() -> Fallible<()> {
        let c = test_config()?;
        let p = PathBuf::from("policy.json");
        Podman::build_args(&c, &p)?;
        Ok(())
    }

    #[test]
    fn build_args_failure() -> Fallible<()> {
        let c = test_config_wrong_root()?;
        let p = PathBuf::from("policy.json");
        assert!(Podman::build_args(&c, &p).is_err());
        Ok(())
    }

    #[test]
    fn default_args_success() -> Fallible<()> {
        let c = test_config()?;
        assert!(!Podman::default_args(&c).is_empty());
        Ok(())
    }
}
