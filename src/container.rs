//! Multi-node container management.
//!
//! Handles building the base container image and running cluster
//! components inside containers for multi-node setups. Each container
//! shares the host network namespace and mounts the runtime root directory.

use crate::{Config, nix::Nix, podman::Podman, process::Process, system::System};
use anyhow::{Result, bail};
use log::{LevelFilter, debug, info, trace};
use std::{
    fmt::Display,
    fs,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

const DEFAULT_IMAGE: &str = "kubernix:base";
const DEFAULT_ROOT: &str = "kubernix";

/// Provides container image building and process execution for multi-node clusters.
pub struct Container;

impl Container {
    /// Build the base image used for the nodes
    pub fn build(config: &Config) -> Result<()> {
        // Verify that the provided runtime exists
        System::find_executable(config.container_runtime())?;

        // Write the policy file
        let policy_json = Self::policy_json(config);
        fs::write(&policy_json, include_str!("assets/policy.json"))?;

        // Nothing needs to be done on single node runs or root users
        if !config.multi_node() {
            return Ok(());
        }

        // Skip rebuild if the image already exists
        if Self::image_exists(config) {
            info!(
                "Container image '{}' already exists, skipping build",
                DEFAULT_IMAGE
            );
            return Ok(());
        }

        // Build the base container image
        info!("Building base container image '{}'", DEFAULT_IMAGE);

        // Prepare the Dockerfile: use a custom one if provided, otherwise
        // fall back to the embedded default.
        let file = config.root().join("Dockerfile");
        if !file.exists() {
            if let Some(custom) = config.dockerfile() {
                debug!("Using custom Dockerfile '{}'", custom.display());
                fs::copy(custom, &file)?;
            } else {
                fs::write(
                    &file,
                    format!(
                        include_str!("assets/Dockerfile"),
                        nix = Nix::DIR,
                        root = DEFAULT_ROOT
                    ),
                )?;
            }
        }

        // Exclude .git from the container build context since the
        // runtime nix directory uses a standalone git repo that the
        // container image does not need.
        let dockerignore = config.root().join(".dockerignore");
        if !dockerignore.exists() {
            fs::write(&dockerignore, "nix/.git\n")?;
        }

        // Prepare the arguments
        let mut args = if Podman::is_configured(config) {
            Podman::build_args(config, &policy_json)?
        } else {
            vec!["build".into()]
        };
        args.extend(vec![format!("-t={}", DEFAULT_IMAGE), ".".into()]);
        trace!("Container runtime build args: {:?}", args);

        // Run the build
        debug!("Running container runtime with args: {}", args.join(" "));
        let status = Command::new(config.container_runtime())
            .current_dir(config.root())
            .args(args)
            .stderr(Self::stdio(config))
            .stdout(Self::stdio(config))
            .status()?;
        if !status.success() {
            bail!(
                "Unable to build container base image '{}' using '{}' (exit: {})",
                DEFAULT_IMAGE,
                config.container_runtime(),
                status,
            );
        }

        info!("Container base image built");
        Ok(())
    }

    /// Retrieve the default signature policy file location
    pub fn policy_json(config: &Config) -> PathBuf {
        config.root().join("policy.json")
    }

    /// Start a new container based process
    pub fn start(
        config: &Config,
        dir: &Path,
        identifier: &str,
        process_name: &str,
        container_name: &str,
        args: &[&str],
    ) -> Result<Process> {
        // Cleanup possible containers
        Self::remove(config, container_name);

        // Prepare the arguments
        let arg_hostname = &format!("--hostname={}", container_name);
        let arg_name = &format!("--name={}", Self::prefixed_container_name(container_name));
        let arg_volume_root = &Self::volume_arg(config.root().display());
        let mut args_vec = vec![
            "run",
            "--net=host",
            "--privileged",
            "--rm",
            "--cgroupns=host",
            arg_hostname,
            arg_name,
            arg_volume_root,
        ];

        // Podman specific arguments
        let podman_args = Podman::default_args(config)?;
        if Podman::is_configured(config) {
            args_vec.extend(podman_args.iter().map(|x| x.as_str()).collect::<Vec<_>>())
        }

        // Mount /dev/mapper if available
        let dev_mapper = PathBuf::from("/").join("dev").join("mapper");
        let arg_volume_dev_mapper = &Self::volume_arg(dev_mapper.display());
        if dev_mapper.exists() {
            args_vec.push(arg_volume_dev_mapper);
        }

        // Add the process and the user provided args
        args_vec.extend(&[DEFAULT_IMAGE, process_name]);
        args_vec.extend(args);

        // Start the process
        trace!("Container runtime start args: {:?}", args_vec);
        Process::start(dir, identifier, config.container_runtime(), &args_vec)
    }

    fn volume_arg<T: Display>(volume: T) -> String {
        format!("--volume={v}:{v}", v = volume)
    }

    /// Exec a command on a container instance
    pub fn exec(
        config: &Config,
        dir: &Path,
        identifier: &str,
        process_name: &str,
        container_name: &str,
        args: &[&str],
    ) -> Result<Process> {
        // Prepare the args
        let mut args_vec = vec![];

        let podman_args = Podman::default_args(config)?;
        if Podman::is_configured(config) {
            args_vec.extend(podman_args.iter().map(|x| x.as_str()).collect::<Vec<_>>())
        }

        let name = Self::prefixed_container_name(container_name);
        let flake_ref = format!("path:{}", DEFAULT_ROOT);
        let mut cmd_parts = vec![process_name.to_string()];
        cmd_parts.extend(args.iter().map(|a| a.to_string()));
        let run_cmd = cmd_parts.join(" ");
        args_vec.extend(vec![
            "exec",
            &name,
            "nix",
            "develop",
            &flake_ref,
            "--no-update-lock-file",
            "--command",
            "bash",
            "-c",
            &run_cmd,
        ]);

        // Run as usual process
        trace!("Container runtime exec args: {:?}", args_vec);
        Process::start(dir, identifier, config.container_runtime(), &args_vec)
    }

    /// Remove the provided (maybe running) container.
    /// Failures are logged but not propagated since the container may not
    /// exist yet (first run).
    fn remove(config: &Config, name: &str) {
        let prefixed = Self::prefixed_container_name(name);
        match Command::new(config.container_runtime())
            .arg("rm")
            .arg("-f")
            .arg(&prefixed)
            .stderr(Stdio::null())
            .stdout(Stdio::null())
            .status()
        {
            Ok(status) if !status.success() => {
                debug!("Container '{}' removal exited with {}", prefixed, status);
            }
            Err(e) => {
                debug!("Failed to run container rm for '{}': {}", prefixed, e);
            }
            _ => {}
        }
    }

    /// Retrieve a stdio for the provided config log level
    fn stdio(config: &Config) -> Stdio {
        if config.log_level() > LevelFilter::Info {
            Stdio::inherit()
        } else {
            Stdio::null()
        }
    }

    /// Check whether the base container image already exists.
    /// Uses `image inspect` which works for both podman and docker.
    fn image_exists(config: &Config) -> bool {
        Command::new(config.container_runtime())
            .args(["image", "inspect", DEFAULT_IMAGE])
            .stderr(Stdio::null())
            .stdout(Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
    }

    /// Retrieve a prefixed container name
    fn prefixed_container_name(name: &str) -> String {
        format!("{}-{}", DEFAULT_ROOT, name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefixed_container_name_format() {
        assert_eq!(
            Container::prefixed_container_name("node-0"),
            "kubernix-node-0"
        );
    }

    #[test]
    fn volume_arg_format() {
        assert_eq!(
            Container::volume_arg("/some/path"),
            "--volume=/some/path:/some/path"
        );
    }

    #[test]
    fn policy_json_path() {
        let c = crate::config::tests::test_config().unwrap();
        let path = Container::policy_json(&c);
        assert!(path.ends_with("policy.json"));
    }
}
