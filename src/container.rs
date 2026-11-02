//! Multi-node container management.
//!
//! Handles preparing the node image and running cluster components inside
//! containers for multi-node setups in root mode. Each container shares the
//! host network namespace, mounts the runtime root directory and the Nix
//! store of the host read-only, so that the components run from the same
//! store paths as on the host.

use crate::{
    Config, content_hash, podman::Podman, process::Process, system::System, write_if_changed,
};
use anyhow::{Context, Result, bail};
use log::{LevelFilter, debug, info, trace};
use std::{
    env::var,
    fmt::Display,
    fs,
    io::{ErrorKind, Write},
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

/// The prefix of the node images, which get tagged with a hash of their
/// content.
const IMAGE_PREFIX: &str = "kubernix:base";

const DEFAULT_ROOT: &str = "kubernix";

/// The Nix store, which gets mounted read-only into the node containers.
const NIX_STORE: &str = "/nix/store";

/// The location of the CA bundle in the node containers, if the one of the
/// host is not part of the Nix store.
const CA_BUNDLE: &str = "/etc/ssl/certs/ca-certificates.crt";

/// The CA bundles which Go programs use if `SSL_CERT_FILE` is not set.
const SYSTEM_CA_BUNDLES: [&str; 6] = [
    "/etc/ssl/certs/ca-certificates.crt",
    "/etc/pki/tls/certs/ca-bundle.crt",
    "/etc/ssl/ca-bundle.pem",
    "/etc/pki/tls/cacert.pem",
    "/etc/pki/ca-trust/extracted/pem/tls-ca-bundle.pem",
    "/etc/ssl/cert.pem",
];

/// Directories of the default node image as (path, mode).
const ROOTFS_DIRS: [(&str, u32); 6] = [
    ("etc", 0o755),
    ("root", 0o700),
    ("run", 0o755),
    ("tmp", 0o1777),
    ("var", 0o755),
    ("var/tmp", 0o1777),
];

/// Files of the default node image as (path, content). Some components
/// look up the user database, for example to resolve the home directory.
const ROOTFS_FILES: [(&str, &str); 2] = [
    (
        "etc/passwd",
        "root:x:0:0:root:/root:/bin/sh\nnobody:x:65534:65534:nobody:/:/bin/sh\n",
    ),
    ("etc/group", "root:x:0:\nnobody:x:65534:\n"),
];

/// Provides node image preparation and process execution for multi-node clusters.
pub struct Container;

impl Container {
    /// Prepare the image used for the nodes
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

        // Older versions wrote a Dockerfile into the run root, which is not
        // needed any more.
        let dockerfile = config.root().join("Dockerfile");
        if let Err(e) = fs::remove_file(&dockerfile)
            && e.kind() != ErrorKind::NotFound
        {
            return Err(e).with_context(|| format!("Unable to remove '{}'", dockerfile.display()));
        }

        // Skip the image creation if it already exists. The tag contains a
        // hash of the default image content or the custom build inputs, so
        // changed inputs result in a new image.
        let image = Self::image(config)?;
        let runtime_args = Self::runtime_args(config)?;
        if Self::image_exists(config, &runtime_args, &image) {
            info!("Container image '{}' already exists, skipping build", image);
            return Ok(());
        }

        if let Some(dockerfile) = config.dockerfile() {
            Self::build_image(config, dockerfile, &policy_json, &image)?;
        } else {
            Self::import_image(config, &policy_json, &image)?;
        }
        Self::prune_images(config, &runtime_args, &image);
        Ok(())
    }

    /// Build the node image from the custom Dockerfile, using its directory
    /// as build context.
    fn build_image(
        config: &Config,
        dockerfile: &Path,
        policy_json: &Path,
        image: &str,
    ) -> Result<()> {
        info!(
            "Building container image '{}' from '{}'",
            image,
            dockerfile.display()
        );
        let mut args = if Podman::is_configured(config) {
            Podman::signature_args(config, "build", policy_json)?
        } else {
            vec!["build".into()]
        };
        args.extend([
            format!("--file={}", dockerfile.display()),
            format!("--tag={image}"),
            Self::build_context(dockerfile)?.display().to_string(),
        ]);

        debug!("Running container runtime with args: {}", args.join(" "));
        let status = Command::new(config.container_runtime())
            .args(args)
            .stderr(Self::stdio(config))
            .stdout(Self::stdio(config))
            .status()?;
        if !status.success() {
            bail!(
                "Unable to build container image '{}' using '{}' (exit: {})",
                image,
                config.container_runtime(),
                status,
            );
        }
        info!("Container image built");
        Ok(())
    }

    /// Import the default node image, which only contains the directories
    /// and files of a minimal root file system, since everything else gets
    /// mounted from the host.
    fn import_image(config: &Config, policy_json: &Path, image: &str) -> Result<()> {
        info!("Creating container image '{}'", image);
        let mut args = if Podman::is_configured(config) {
            Podman::signature_args(config, "import", policy_json)?
        } else {
            vec!["import".into()]
        };
        args.extend(["-".into(), image.into()]);

        debug!("Running container runtime with args: {}", args.join(" "));
        let mut child = Command::new(config.container_runtime())
            .args(args)
            .stdin(Stdio::piped())
            .stderr(Self::stdio(config))
            .stdout(Self::stdio(config))
            .spawn()?;

        let written = child
            .stdin
            .take()
            .context("Unable to access container runtime stdin")?
            .write_all(&Self::rootfs());
        let status = child.wait()?;
        written.context("Unable to write image archive")?;
        if !status.success() {
            bail!(
                "Unable to import container image '{}' using '{}' (exit: {})",
                image,
                config.container_runtime(),
                status,
            );
        }
        Ok(())
    }

    /// The default node image content as tar archive.
    fn rootfs() -> Vec<u8> {
        let mut archive = vec![];
        for (path, mode) in ROOTFS_DIRS {
            Self::tar_entry(&mut archive, &format!("{}/", path), mode, b'5', b"");
        }
        for (path, content) in ROOTFS_FILES {
            Self::tar_entry(&mut archive, path, 0o644, b'0', content.as_bytes());
        }
        // The end of the archive consists of two zero filled blocks
        archive.resize(archive.len() + 1024, 0);
        archive
    }

    /// Append a ustar entry owned by root to the archive.
    fn tar_entry(archive: &mut Vec<u8>, path: &str, mode: u32, kind: u8, content: &[u8]) {
        let mut header = [0u8; 512];
        let mut field = |offset: usize, value: &[u8]| {
            header[offset..offset + value.len()].copy_from_slice(value);
        };
        field(0, path.as_bytes());
        field(100, format!("{:07o}\0", mode).as_bytes());
        field(108, b"0000000\0");
        field(116, b"0000000\0");
        field(124, format!("{:011o}\0", content.len()).as_bytes());
        field(136, b"00000000000\0");
        field(148, b"        ");
        field(156, &[kind]);
        field(257, b"ustar\0");
        field(263, b"00");
        field(265, b"root");
        field(297, b"root");
        let checksum: u32 = header.iter().map(|b| u32::from(*b)).sum();
        header[148..156].copy_from_slice(format!("{:06o}\0 ", checksum).as_bytes());
        archive.extend_from_slice(&header);
        archive.extend_from_slice(content);
        archive.resize(archive.len().next_multiple_of(512), 0);
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
        let arg_volume_store = &format!("{}:ro", Self::volume_arg(NIX_STORE));
        let arg_entrypoint = &format!(
            "--entrypoint={}",
            System::find_executable(process_name)?.display()
        );
        let arg_env = Self::env_args();
        let arg_ca_bundle = Self::ca_bundle_args();
        let mut args_vec = vec![
            "run",
            "--net=host",
            "--privileged",
            "--rm",
            "--cgroupns=host",
            arg_hostname,
            arg_name,
            arg_volume_root,
            arg_volume_store,
            arg_entrypoint,
        ];
        args_vec.extend(arg_env.iter().map(String::as_str));
        args_vec.extend(arg_ca_bundle.iter().map(String::as_str));

        // Podman specific arguments
        args_vec.extend(runtime_args.iter().map(String::as_str));

        // Mount /dev/mapper if available (requires privileges)
        let dev_mapper = PathBuf::from("/dev/mapper");
        let arg_volume_dev_mapper = &Self::volume_arg(dev_mapper.display());
        if !config.is_rootless() && dev_mapper.exists() {
            args_vec.push(arg_volume_dev_mapper);
        }

        // Add the image and the user provided args
        let image = Self::image(config)?;
        args_vec.push(&image);
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

    /// The environment of the node containers. The processes run from the
    /// Nix store of the host, so they use the `PATH` of the Nix environment
    /// as well.
    fn env_args() -> Vec<String> {
        var("PATH")
            .map(|path| vec![format!("--env=PATH={}", path)])
            .unwrap_or_default()
    }

    /// The arguments which provide the CA bundle for pulling images to the
    /// node containers, since the node image may not contain one.
    fn ca_bundle_args() -> Vec<String> {
        Self::ca_bundle_args_from(
            |key| var(key).ok(),
            |path| fs::canonicalize(path).ok().filter(|p| p.is_file()),
        )
    }

    /// Select the CA bundle which the components use on the host: the
    /// `SSL_CERT_FILE`, a system bundle or the one of the Nix environment.
    /// A bundle outside of the Nix store gets mounted read-only.
    fn ca_bundle_args_from<F, R>(get: F, resolve: R) -> Vec<String>
    where
        F: Fn(&str) -> Option<String>,
        R: Fn(&Path) -> Option<PathBuf>,
    {
        let bundle = get("SSL_CERT_FILE")
            .into_iter()
            .chain(SYSTEM_CA_BUNDLES.iter().map(|b| b.to_string()))
            .chain(get("NIX_SSL_CERT_FILE"))
            .find_map(|bundle| resolve(Path::new(&bundle)));
        match bundle {
            Some(bundle) if bundle.starts_with(NIX_STORE) => {
                vec![format!("--env=SSL_CERT_FILE={}", bundle.display())]
            }
            Some(bundle) => vec![
                format!("--volume={}:{}:ro", bundle.display(), CA_BUNDLE),
                format!("--env=SSL_CERT_FILE={}", CA_BUNDLE),
            ],
            None => vec![],
        }
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
        let executable = System::find_executable(process_name)?.display().to_string();
        args_vec.extend(["exec", &name, &executable]);
        args_vec.extend(args);

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

    /// The node image name, tagged with a hash of the default image content
    /// or of the custom build inputs.
    fn image(config: &Config) -> Result<String> {
        let Some(dockerfile) = config.dockerfile() else {
            return Ok(Self::image_name(&[&Self::rootfs()]));
        };
        let mut inputs = vec![
            fs::read(dockerfile)
                .with_context(|| format!("Unable to read '{}'", dockerfile.display()))?,
        ];
        let context = Self::build_context(dockerfile)?;
        let root = fs::canonicalize(config.root()).unwrap_or_else(|_| config.root().into());
        Self::context_inputs(&context, &context, &root, &mut inputs)?;
        let inputs: Vec<&[u8]> = inputs.iter().map(Vec::as_slice).collect();
        Ok(Self::image_name(&inputs))
    }

    /// The image name for the provided build inputs.
    fn image_name(inputs: &[&[u8]]) -> String {
        format!("{}-{:016x}", IMAGE_PREFIX, content_hash(inputs))
    }

    /// The build context of a custom Dockerfile, which is its directory.
    fn build_context(dockerfile: &Path) -> Result<PathBuf> {
        let dockerfile = fs::canonicalize(dockerfile)
            .with_context(|| format!("Unable to resolve '{}'", dockerfile.display()))?;
        dockerfile
            .parent()
            .map(Path::to_path_buf)
            .context("Dockerfile has no parent directory")
    }

    /// Collect the metadata of all entries in the build context
    /// recursively, except the run root. The metadata changes together with
    /// the content and is cheaper to read.
    fn context_inputs(
        context: &Path,
        dir: &Path,
        exclude: &Path,
        inputs: &mut Vec<Vec<u8>>,
    ) -> Result<()> {
        let mut entries = fs::read_dir(dir)
            .with_context(|| format!("Unable to read build context '{}'", dir.display()))?
            .collect::<Result<Vec<_>, _>>()
            .with_context(|| format!("Unable to read build context '{}'", dir.display()))?;
        entries.sort_by_key(|entry| entry.file_name());
        for entry in entries {
            let path = entry.path();
            if path == exclude {
                continue;
            }
            let metadata = entry
                .metadata()
                .with_context(|| format!("Unable to read '{}'", path.display()))?;
            inputs.push(
                format!(
                    "{} {:o} {} {}.{}",
                    path.strip_prefix(context).unwrap_or(&path).display(),
                    metadata.mode(),
                    metadata.len(),
                    metadata.mtime(),
                    metadata.mtime_nsec(),
                )
                .into_bytes(),
            );
            if metadata.is_dir() {
                Self::context_inputs(context, &path, exclude, inputs)?;
            }
        }
        Ok(())
    }

    /// Check whether the node image already exists.
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

    /// Remove the node images of previous builds and older versions. Images
    /// which are still in use, for example by another run root, fail to be
    /// removed.
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

    /// Filter the listed `repository:tag` images for `kubernix:base-*`
    /// images other than the provided one. Podman prefixes the repository with
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
        let a = Container::image_name(&[b"FROM scratch"]);
        assert!(a.starts_with("kubernix:base-"));
        assert_eq!(a, Container::image_name(&[b"FROM scratch"]));
        assert_ne!(a, Container::image_name(&[b"FROM alpine"]));
    }

    #[test]
    fn image_custom_depends_on_build_context() -> Result<()> {
        let context = tempfile::tempdir()?;
        let dockerfile = context.path().join("Dockerfile");
        fs::write(&dockerfile, "FROM scratch\nCOPY file /\n")?;
        fs::write(context.path().join("file"), "a")?;
        let root = context.path().join("kubernix-run");
        fs::create_dir(&root)?;
        let config = || {
            Config::parse_from([
                "kubernix",
                "--nodes=2",
                &format!("--root={}", root.display()),
                &format!("--dockerfile={}", dockerfile.display()),
            ])
        };
        let image = Container::image(&config())?;
        assert!(image.starts_with("kubernix:base-"));
        assert_ne!(image, Container::image(&Config::parse_from(["kubernix"]))?);
        assert_eq!(image, Container::image(&config())?);

        // The run root is excluded
        fs::write(root.join("state"), "changed")?;
        assert_eq!(image, Container::image(&config())?);

        // Files in the context and the Dockerfile are included
        fs::create_dir(context.path().join("dir"))?;
        fs::write(context.path().join("dir").join("nested"), "b")?;
        let nested = Container::image(&config())?;
        assert_ne!(image, nested);
        fs::write(context.path().join("dir").join("nested"), "bc")?;
        assert_ne!(nested, Container::image(&config())?);
        fs::write(&dockerfile, "FROM alpine\n")?;
        assert_ne!(nested, Container::image(&config())?);
        Ok(())
    }

    #[test]
    fn build_context_is_dockerfile_dir() -> Result<()> {
        let context = tempfile::tempdir()?;
        let dockerfile = context.path().join("Dockerfile");
        fs::write(&dockerfile, "FROM scratch\n")?;
        assert_eq!(
            Container::build_context(&dockerfile)?,
            fs::canonicalize(context.path())?
        );
        assert!(Container::build_context(&context.path().join("missing")).is_err());
        Ok(())
    }

    #[test]
    fn image_default_is_rootfs() -> Result<()> {
        let c = Config::parse_from(["kubernix", "--nodes=2"]);
        assert_eq!(
            Container::image(&c)?,
            Container::image_name(&[&Container::rootfs()])
        );
        Ok(())
    }

    #[test]
    fn rootfs_is_valid_tar() {
        let archive = Container::rootfs();
        assert_eq!(archive.len() % 512, 0);
        assert!(archive.ends_with(&[0; 1024]));

        // The first entry is a directory with a valid checksum
        let header = &archive[..512];
        assert_eq!(&header[..4], b"etc/");
        assert_eq!(header[156], b'5');
        assert_eq!(&header[257..263], b"ustar\0");
        let stored = std::str::from_utf8(&header[148..154]).unwrap();
        let mut copy = header.to_vec();
        copy[148..156].copy_from_slice(b"        ");
        let sum: u32 = copy.iter().map(|b| u32::from(*b)).sum();
        assert_eq!(u32::from_str_radix(stored, 8).unwrap(), sum);

        // The passwd file contains root
        let passwd = archive
            .chunks(512)
            .position(|block| block.starts_with(b"etc/passwd\0"))
            .unwrap();
        assert_eq!(archive[passwd * 512 + 156], b'0');
        assert!(archive[(passwd + 1) * 512..].starts_with(b"root:x:0:0:"));
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
    fn env_args_forward_path() {
        let path = var("PATH").unwrap_or_default();
        assert_eq!(Container::env_args(), vec![format!("--env=PATH={}", path)]);
    }

    fn ca_bundle_args(vars: &[(&str, &str)], files: &[(&str, &str)]) -> Vec<String> {
        Container::ca_bundle_args_from(
            |key| {
                vars.iter()
                    .find(|(k, _)| *k == key)
                    .map(|(_, v)| v.to_string())
            },
            |path| {
                files
                    .iter()
                    .find(|(f, _)| Path::new(f) == path)
                    .map(|(_, target)| PathBuf::from(target))
            },
        )
    }

    #[test]
    fn ca_bundle_args_prefers_ssl_cert_file() {
        // A host bundle gets mounted to a fixed location
        assert_eq!(
            ca_bundle_args(
                &[
                    ("SSL_CERT_FILE", "/etc/custom.crt"),
                    ("NIX_SSL_CERT_FILE", "/nix/store/c/ca.crt"),
                ],
                &[
                    ("/etc/custom.crt", "/etc/custom.crt"),
                    (
                        "/etc/ssl/certs/ca-certificates.crt",
                        "/etc/ssl/certs/ca.crt"
                    ),
                    ("/nix/store/c/ca.crt", "/nix/store/c/ca.crt"),
                ],
            ),
            vec![
                "--volume=/etc/custom.crt:/etc/ssl/certs/ca-certificates.crt:ro",
                "--env=SSL_CERT_FILE=/etc/ssl/certs/ca-certificates.crt",
            ]
        );

        // A missing SSL_CERT_FILE falls back to the system bundle
        assert_eq!(
            ca_bundle_args(
                &[("SSL_CERT_FILE", "/etc/missing.crt")],
                &[("/etc/pki/tls/certs/ca-bundle.crt", "/etc/pki/ca.crt")],
            ),
            vec![
                "--volume=/etc/pki/ca.crt:/etc/ssl/certs/ca-certificates.crt:ro",
                "--env=SSL_CERT_FILE=/etc/ssl/certs/ca-certificates.crt",
            ]
        );
    }

    #[test]
    fn ca_bundle_args_uses_store_paths_directly() {
        // A system bundle which resolves into the Nix store, like on NixOS
        assert_eq!(
            ca_bundle_args(
                &[("NIX_SSL_CERT_FILE", "/nix/store/c/ca.crt")],
                &[
                    ("/etc/ssl/certs/ca-certificates.crt", "/nix/store/s/ca.crt"),
                    ("/nix/store/c/ca.crt", "/nix/store/c/ca.crt"),
                ],
            ),
            vec!["--env=SSL_CERT_FILE=/nix/store/s/ca.crt"]
        );

        // The bundle of the Nix environment if the host has none
        assert_eq!(
            ca_bundle_args(
                &[("NIX_SSL_CERT_FILE", "/nix/store/c/ca.crt")],
                &[("/nix/store/c/ca.crt", "/nix/store/c/ca.crt")],
            ),
            vec!["--env=SSL_CERT_FILE=/nix/store/c/ca.crt"]
        );
        assert!(ca_bundle_args(&[], &[]).is_empty());
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
