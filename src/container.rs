//! Multi-node container management.
//!
//! Handles building the base container image and running cluster
//! components inside containers for multi-node setups in root mode. Each
//! container shares the host network namespace and mounts the runtime root
//! directory.

use crate::{Config, nix::Nix, podman::Podman, process::Process, system::System, write_if_changed};
use anyhow::{Context, Result, bail};
use log::{LevelFilter, debug, info, trace};
use std::{
    fmt::Display,
    fs,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

const IMAGE_PREFIX: &str = "kubernix:base";
const DEFAULT_ROOT: &str = "kubernix";

/// The base image of the default Dockerfile.
const BASE_IMAGE: &str = "docker.io/nixos/nix:latest";

/// Provides container image building and process execution for multi-node clusters.
pub struct Container;

impl Container {
    /// Build the base image used for the nodes
    pub fn build(config: &Config) -> Result<()> {
        // Verify that the provided runtime exists
        System::find_executable(config.container_runtime())?;

        // Write the policy file
        let policy_json = Self::policy_json(config);
        write_if_changed(&policy_json, include_str!("assets/policy.json"))
            .context("Unable to write container signature policy")?;

        // Nothing needs to be done on single node or rootless runs
        if !config.multi_node() || config.is_rootless() {
            return Ok(());
        }

        // Prepare the Dockerfile: use a custom one if provided, otherwise
        // fall back to the embedded default. It gets refreshed on every run,
        // so that the image tag reflects the current content.
        let dockerfile = match config.dockerfile() {
            Some(custom) => {
                debug!("Using custom Dockerfile '{}'", custom.display());
                fs::read_to_string(custom).with_context(|| {
                    format!("Unable to read custom Dockerfile '{}'", custom.display())
                })?
            }
            None => format!(
                include_str!("assets/Dockerfile"),
                base = BASE_IMAGE,
                nix = Nix::DIR,
                root = DEFAULT_ROOT
            ),
        };
        write_if_changed(&config.root().join("Dockerfile"), &dockerfile)
            .context("Unable to write Dockerfile")?;

        // Skip rebuild if the image for the current environment already
        // exists. The tag contains a hash of the build inputs, so a changed
        // Nix environment results in a new image.
        let image = Self::image(config)?;
        let runtime_args = Self::runtime_args(config)?;
        if Self::image_exists(config, &runtime_args, &image) {
            info!("Container image '{}' already exists, skipping build", image);
            return Ok(());
        }

        // Build the base container image
        info!("Building base container image '{}'", image);

        // Prepare the arguments
        let mut args = if Podman::is_configured(config) {
            Podman::build_args(config, &policy_json)?
        } else {
            vec!["build".into()]
        };
        args.extend([format!("-t={image}"), ".".into()]);
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
                image,
                config.container_runtime(),
                status,
            );
        }

        info!("Container base image built");
        Self::prune_images(config, &runtime_args, &image);
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
        let runtime_args = Self::runtime_args(config)?;

        // Cleanup possible containers
        Self::remove(config, &runtime_args, container_name);

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
        args_vec.extend(runtime_args.iter().map(String::as_str));

        // Mount /dev/mapper if available (requires privileges)
        let dev_mapper = PathBuf::from("/dev/mapper");
        let arg_volume_dev_mapper = &Self::volume_arg(dev_mapper.display());
        if !config.is_rootless() && dev_mapper.exists() {
            args_vec.push(arg_volume_dev_mapper);
        }

        // Add the process and the user provided args
        let image = Self::image(config)?;
        args_vec.extend(&[image.as_str(), process_name]);
        args_vec.extend(args);

        // Start the process
        trace!("Container runtime start args: {:?}", args_vec);
        Process::start(dir, identifier, config.container_runtime(), &args_vec)
    }

    /// The handle of the node container for the provided node, if the
    /// node runs inside a container.
    pub fn node(config: &Config, container_name: &str) -> Option<NodeContainer> {
        if !config.multi_node() || config.is_rootless() {
            return None;
        }
        Some(NodeContainer {
            runtime: config.container_runtime().into(),
            runtime_args: Self::runtime_args(config).unwrap_or_default(),
            name: container_name.into(),
            removed: false,
        })
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
        let runtime_args = Self::runtime_args(config)?;
        let mut args_vec: Vec<&str> = runtime_args.iter().map(String::as_str).collect();

        let name = Self::prefixed_container_name(container_name);
        let flake_ref = format!("path:{}", DEFAULT_ROOT);
        let mut cmd_parts = vec![process_name.to_string()];
        cmd_parts.extend(args.iter().map(|a| a.to_string()));
        let run_cmd = cmd_parts.join(" ");
        args_vec.extend([
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

    /// The arguments which apply to every container runtime command, only
    /// podman has some.
    fn runtime_args(config: &Config) -> Result<Vec<String>> {
        if Podman::is_configured(config) {
            Podman::default_args(config)
        } else {
            Ok(vec![])
        }
    }

    /// Remove the provided (maybe running) container.
    /// Failures are logged but not propagated since the container may not
    /// exist yet (first run).
    fn remove(config: &Config, runtime_args: &[String], name: &str) {
        let prefixed = Self::prefixed_container_name(name);
        Self::run_quietly(
            config.container_runtime(),
            runtime_args,
            &["rm", "-f", &prefixed],
        );
    }

    /// Run the container runtime, logging failures only, since they are
    /// expected, for example if a container does not exist.
    fn run_quietly(runtime: &str, runtime_args: &[String], args: &[&str]) {
        match Command::new(runtime)
            .args(runtime_args)
            .args(args)
            .stderr(Stdio::null())
            .stdout(Stdio::null())
            .status()
        {
            Ok(status) if !status.success() => {
                debug!("'{} {}' exited with {}", runtime, args.join(" "), status);
            }
            Err(e) => {
                debug!("Failed to run '{} {}': {}", runtime, args.join(" "), e);
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

    /// The base image name, tagged with a hash of the Nix environment, the
    /// Dockerfile and its default base image, which are the inputs of the
    /// image build.
    fn image(config: &Config) -> Result<String> {
        let nix_dir = config.root().join(Nix::DIR);
        let mut inputs = vec![];
        for file in Nix::FILES {
            let path = nix_dir.join(file);
            inputs.push(
                fs::read(&path).with_context(|| format!("Unable to read '{}'", path.display()))?,
            );
        }
        let dockerfile = config.root().join("Dockerfile");
        inputs.push(
            fs::read(&dockerfile)
                .with_context(|| format!("Unable to read '{}'", dockerfile.display()))?,
        );
        inputs.push(BASE_IMAGE.as_bytes().to_vec());
        Ok(Self::image_name(&inputs))
    }

    /// The base image name for the provided build inputs.
    fn image_name(inputs: &[Vec<u8>]) -> String {
        // FNV-1a, stable across Rust versions and platforms.
        let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
        for input in inputs {
            for byte in (input.len() as u64).to_le_bytes().iter().chain(input) {
                hash ^= u64::from(*byte);
                hash = hash.wrapping_mul(0x0100_0000_01b3);
            }
        }
        format!("{}-{:016x}", IMAGE_PREFIX, hash)
    }

    /// Check whether the base container image already exists.
    /// Uses `image inspect` which works for both podman and docker.
    fn image_exists(config: &Config, runtime_args: &[String], image: &str) -> bool {
        Command::new(config.container_runtime())
            .args(runtime_args)
            .args(["image", "inspect", image])
            .stderr(Stdio::null())
            .stdout(Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
    }

    /// Remove the base images of previous builds. Images which are still
    /// in use, for example by another run root, fail to be removed.
    fn prune_images(config: &Config, runtime_args: &[String], image: &str) {
        let output = match Command::new(config.container_runtime())
            .args(runtime_args)
            .args(["images", "--format", "{{.Repository}}:{{.Tag}}"])
            .stderr(Stdio::null())
            .output()
        {
            Ok(output) if output.status.success() => output,
            Ok(output) => {
                debug!("Unable to list container images: {}", output.status);
                return;
            }
            Err(e) => {
                debug!("Unable to list container images: {}", e);
                return;
            }
        };
        for old in Self::stale_images(&String::from_utf8_lossy(&output.stdout), image) {
            debug!("Removing old container image '{}'", old);
            Self::run_quietly(config.container_runtime(), runtime_args, &["rmi", &old]);
        }
    }

    /// Filter the listed `repository:tag` images for base images other
    /// than the provided one. Podman prefixes the repository with
    /// `localhost/`.
    fn stale_images(list: &str, image: &str) -> Vec<String> {
        let (repo, prefix) = IMAGE_PREFIX.split_once(':').unwrap_or_default();
        let current = image.rsplit_once(':').map(|(_, tag)| tag);
        let mut images: Vec<String> = list
            .lines()
            .map(str::trim)
            .filter(|line| {
                line.rsplit_once(':').is_some_and(|(r, tag)| {
                    (r == repo || r.ends_with(&format!("/{}", repo)))
                        && tag.starts_with(prefix)
                        && Some(tag) != current
                })
            })
            .map(String::from)
            .collect();
        images.sort();
        images.dedup();
        images
    }

    /// Retrieve a prefixed container name
    fn prefixed_container_name(name: &str) -> String {
        format!("{}-{}", DEFAULT_ROOT, name)
    }
}

/// A node container, which runs the CRI runtime of a node in multi-node
/// mode. It gets removed when stopped or dropped.
pub struct NodeContainer {
    runtime: String,
    runtime_args: Vec<String>,
    name: String,
    removed: bool,
}

impl NodeContainer {
    /// Stop and remove the container. Stopping the container runtime
    /// client is not sufficient, since the container keeps running if its
    /// entrypoint does not handle the forwarded signal.
    pub fn remove(&mut self) {
        if self.removed {
            return;
        }
        self.removed = true;
        let name = Container::prefixed_container_name(&self.name);
        debug!("Removing node container {}", name);
        Container::run_quietly(
            &self.runtime,
            &self.runtime_args,
            &["stop", "-t", "10", &name],
        );
        Container::run_quietly(&self.runtime, &self.runtime_args, &["rm", "-f", &name]);
    }
}

impl Drop for NodeContainer {
    fn drop(&mut self) {
        self.remove();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[test]
    fn prefixed_container_name_format() {
        assert_eq!(
            Container::prefixed_container_name("node-0"),
            "kubernix-node-0"
        );
    }

    #[test]
    fn image_name_depends_on_inputs() {
        let a = Container::image_name(&[b"crun".to_vec(), b"df".to_vec()]);
        assert!(a.starts_with("kubernix:base-"));
        assert_eq!(
            a,
            Container::image_name(&[b"crun".to_vec(), b"df".to_vec()])
        );
        assert_ne!(
            a,
            Container::image_name(&[b"crun runc".to_vec(), b"df".to_vec()])
        );
        assert_ne!(
            a,
            Container::image_name(&[b"crund".to_vec(), b"f".to_vec()])
        );
    }

    #[test]
    fn stale_images_filters_old_base_images() {
        let list = "\
            localhost/kubernix:base-0000000000000001\n\
            localhost/kubernix:base-0000000000000002\n\
            kubernix:base-0000000000000003\n\
            kubernix:base-0000000000000003\n\
            localhost/kubernix:latest\n\
            docker.io/library/kubernix-other:base-1\n\
            docker.io/nixos/nix:latest\n";
        assert_eq!(
            Container::stale_images(list, "kubernix:base-0000000000000002"),
            vec![
                "kubernix:base-0000000000000003",
                "localhost/kubernix:base-0000000000000001",
            ]
        );
        assert!(Container::stale_images("", "kubernix:base-1").is_empty());
    }

    #[test]
    fn default_dockerfile_uses_base_image() {
        let dockerfile = format!(
            include_str!("assets/Dockerfile"),
            base = BASE_IMAGE,
            nix = Nix::DIR,
            root = DEFAULT_ROOT
        );
        assert!(dockerfile.starts_with(&format!("FROM {}\n", BASE_IMAGE)));
    }

    #[test]
    fn node_container_only_for_multi_node_root() -> Result<()> {
        let mut c = crate::config::tests::test_config()?;
        assert!(Container::node(&c, "node-0").is_none());
        c = Config::parse_from(["kubernix", "--nodes=2", "--container-runtime=true"]);
        c.set_rootless(true);
        assert!(Container::node(&c, "node-0").is_none());
        c.set_rootless(false);
        let mut node = Container::node(&c, "node-0").context("missing node container")?;
        assert_eq!(node.name, "node-0");
        node.remove();
        assert!(node.removed);
        Ok(())
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
