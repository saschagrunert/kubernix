//! Multi-node container management.
//!
//! Handles preparing the node image and running cluster components inside
//! containers for multi-node setups in root mode. Each container shares the
//! host network namespace, mounts the runtime root directory and the Nix
//! store of the host read-only, so that the components run from the same
//! store paths as on the host.

use crate::{
    Config, content_hash,
    network::{Network, SYSTEMD_RESOLV_CONF},
    podman::Podman,
    process::{self, Process},
    signals,
    system::System,
    write_file, write_if_changed,
};
use anyhow::{Context, Result, bail};
use log::{LevelFilter, debug, info, trace, warn};
use std::{
    env::{var, var_os},
    ffi::OsStr,
    fmt::Display,
    fs,
    io::ErrorKind,
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
    sync::OnceLock,
};

/// The prefix of the node images, which get tagged with a hash of their
/// content.
const IMAGE_PREFIX: &str = "kubernix:base";

/// The version of the inputs of custom node images, which is part of their
/// hash. It changes if images of older versions must not be used anymore,
/// version 2 since run directories are no longer part of the build context.
const CUSTOM_IMAGE_VERSION: &[u8] = b"v2";

/// The node image of this run, which gets determined once by `build`, since
/// the build context may change while the cluster runs.
static IMAGE: OnceLock<String> = OnceLock::new();

/// The prefix of the node container names, followed by the id of the run
/// root and the node name.
const NAME_PREFIX: &str = "kubernix";

/// The label which contains the id of the run root of a node container.
const ROOT_LABEL: &str = "kubernix.root";

/// The label which contains the path of the run root of a node container.
const ROOT_PATH_LABEL: &str = "kubernix.root.path";

/// The proxy variables which get forwarded to the node containers. Podman
/// forwards them by default, but Docker does not.
const PROXY_VARS: [&str; 6] = [
    "HTTP_PROXY",
    "HTTPS_PROXY",
    "NO_PROXY",
    "http_proxy",
    "https_proxy",
    "no_proxy",
];

/// The registry credentials of root, which CRI-O reads on the host and
/// which get mounted read-only into the node containers if they exist.
const REGISTRY_AUTH_FILES: [&str; 3] = [
    "/run/containers/0/auth.json",
    "/root/.config/containers/auth.json",
    "/root/.docker/config.json",
];

/// The variable which overrides the location of the registry credentials.
const REGISTRY_AUTH_FILE: &str = "REGISTRY_AUTH_FILE";

/// The registry certificate directories, which CRI-O and containerd read on
/// the host and which get mounted read-only into the node containers if they
/// exist.
const REGISTRY_CERT_DIRS: [&str; 3] = [
    "/etc/containers/certs.d",
    "/etc/containerd/certs.d",
    "/etc/docker/certs.d",
];

/// The amount of error output lines of a failed container runtime command
/// which get included in the error.
const STDERR_TAIL_LINES: usize = 20;

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

        // The stale containers of the run directory are gone already, see
        // `remove_stale_on_start`.
        if let Err(e) = Self::remove_legacy(config, var_os("PATH").as_deref()) {
            warn!(
                "Unable to remove the node containers of older versions: {:#}",
                e
            );
        }

        // Check the build context before an existing image gets used, so
        // that a context with the keys of the clusters gets refused even if
        // its image exists.
        let exclude_args = match config.dockerfile() {
            Some(dockerfile) => Self::exclude_root_args(config, &Self::build_context(dockerfile)?)?,
            None => vec![],
        };

        // Skip the image creation if it already exists. The tag contains a
        // hash of the default image content or the custom build inputs, so
        // changed inputs result in a new image. The nodes use the same tag,
        // even if the build context changes afterwards.
        let image = Self::image(config)?;
        let image = IMAGE.get_or_init(|| image);
        let runtime_args = Self::runtime_args(config)?;
        if Self::image_exists(config, &runtime_args, image) {
            info!("Container image '{}' already exists, skipping build", image);
            return Ok(());
        }

        if let Some(dockerfile) = config.dockerfile() {
            Self::build_image(config, dockerfile, &policy_json, image, exclude_args)?;
        } else {
            Self::import_image(config, &policy_json, image)?;
        }
        Self::prune_images(config, &runtime_args, image);
        Ok(())
    }

    /// Remove the node containers of a previous run in the run directory
    /// on startup, which did not get cleaned up, for example because
    /// kubernix got killed. Their kubelets still listen on their ports, so
    /// this has to happen before the port check. The lock of the run
    /// directory ensures that they do not belong to a running instance.
    pub fn remove_stale_on_start(config: &Config) {
        if !config.multi_node() || config.is_rootless() {
            return;
        }
        if let Err(e) = Self::remove_stale(config, var_os("PATH").as_deref()) {
            warn!("Unable to remove stale node containers: {:#}", e);
        }
    }

    /// Remove the node containers of the run root which are left over, for
    /// example if kubernix got killed. The container runtime and crun get
    /// looked up in the provided `PATH`, which the runtime uses as well.
    pub fn remove_stale(config: &Config, path: Option<&OsStr>) -> Result<()> {
        let command = Self::runtime_command(config, path)?;
        let filter = format!(
            "--filter=label={}={}",
            ROOT_LABEL,
            System::root_id(config.root())
        );
        let output = command(&["ps", "--all", "--quiet", &filter])?;
        Self::check_output(&output, "Unable to list the node containers")?;
        let stdout = String::from_utf8_lossy(&output.stdout);
        let ids: Vec<&str> = stdout.split_whitespace().collect();
        if ids.is_empty() {
            return Ok(());
        }

        info!("Removing {} stale node container(s)", ids.len());
        let mut args = vec!["rm", "--force"];
        args.extend(ids);
        Self::check_output(&command(&args)?, "Unable to remove stale node containers")
    }

    /// Remove the node containers of versions before 1.0, which are named
    /// `kubernix-node-<n>` without the id of the run root and carry no
    /// labels, so `remove_stale` does not find them. Only call this on
    /// startup, when the port check ensured that no other cluster runs.
    fn remove_legacy(config: &Config, path: Option<&OsStr>) -> Result<()> {
        let names = Self::legacy_containers(config, path, true)?;
        if names.is_empty() {
            return Ok(());
        }

        info!(
            "Removing {} node container(s) of an older version",
            names.len()
        );
        let mut args = vec!["rm", "--force"];
        args.extend(names.iter().map(String::as_str));
        Self::check_output(
            &Self::runtime_command(config, path)?(&args)?,
            "Unable to remove the node containers of an older version",
        )
    }

    /// The names of the node containers of versions before 1.0, all of
    /// them or only the running ones.
    fn legacy_containers(config: &Config, path: Option<&OsStr>, all: bool) -> Result<Vec<String>> {
        let command = Self::runtime_command(config, path)?;
        let format = "--format={{.Names}}";
        let args: &[&str] = if all {
            &["ps", "--all", format]
        } else {
            &["ps", format]
        };
        let output = command(args)?;
        Self::check_output(&output, "Unable to list the containers")?;
        Ok(Self::legacy_names(&String::from_utf8_lossy(&output.stdout)))
    }

    /// Explain the failed port check of a multi-node cluster in root mode
    /// if node containers of versions before 1.0 run, whose kubelets may
    /// hold the ports. They do not get removed automatically, since they
    /// may belong to a running cluster of an older version.
    pub fn explain_port_conflict(config: &Config, error: anyhow::Error) -> anyhow::Error {
        if !config.multi_node() || config.is_rootless() {
            return error;
        }
        match Self::legacy_containers(config, var_os("PATH").as_deref(), false) {
            Ok(names) if !names.is_empty() => {
                error.context(Self::legacy_hint(config.container_runtime(), &names))
            }
            Ok(_) => error,
            Err(e) => {
                debug!(
                    "Unable to list the node containers of older versions: {:#}",
                    e
                );
                error
            }
        }
    }

    /// The hint for the running node containers `names` of versions before
    /// 1.0, which the container runtime `runtime` runs.
    fn legacy_hint(runtime: &str, names: &[String]) -> String {
        format!(
            "Node containers of a kubernix version before 1.0 are running ({}) and may use \
             the ports. If their cluster is not needed anymore, remove them with: {} rm -f {}",
            names.join(", "),
            runtime,
            names.join(" ")
        )
    }

    /// Filter the listed container names for the ones of versions before
    /// 1.0, which are exactly `kubernix-node-<n>`. Docker separates
    /// multiple names of a container by commas.
    fn legacy_names(list: &str) -> Vec<String> {
        let prefix = format!("{}-node-", NAME_PREFIX);
        let mut names: Vec<String> = list
            .split(|c: char| c.is_whitespace() || c == ',')
            .filter(|name| {
                name.strip_prefix(&prefix)
                    .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
            })
            .map(String::from)
            .collect();
        names.sort();
        names.dedup();
        names
    }

    /// A function which runs the container runtime with the provided
    /// arguments and returns its output. The container runtime and crun
    /// get looked up in the provided `PATH`, which the runtime uses as
    /// well.
    fn runtime_command(
        config: &Config,
        path: Option<&OsStr>,
    ) -> Result<impl Fn(&[&str]) -> Result<Output>> {
        let runtime = System::find_executable_in(config.container_runtime(), path)?;
        let runtime_args = Self::runtime_args_in(config, path)?;
        let path = path.map(OsStr::to_os_string);
        Ok(move |args: &[&str]| {
            let mut cmd = process::command(&runtime);
            if let Some(path) = &path {
                cmd.env("PATH", path);
            }
            cmd.args(&runtime_args)
                .args(args)
                .stdin(Stdio::null())
                .stderr(Stdio::piped())
                .output()
                .with_context(|| format!("Unable to run '{}'", runtime.display()))
        })
    }

    /// Fail with the tail of the error output if the command failed.
    fn check_output(output: &Output, message: &str) -> Result<()> {
        if !output.status.success() {
            bail!(
                "{} (exit: {}){}",
                message,
                output.status,
                Self::stderr_tail(&output.stderr)
            );
        }
        Ok(())
    }

    /// The last lines of the error output of a container runtime command,
    /// prefixed for appending them to an error message.
    fn stderr_tail(stderr: &[u8]) -> String {
        let stderr = String::from_utf8_lossy(stderr);
        let lines: Vec<&str> = stderr
            .lines()
            .map(str::trim_end)
            .filter(|line| !line.is_empty())
            .collect();
        let tail = &lines[lines.len().saturating_sub(STDERR_TAIL_LINES)..];
        if tail.is_empty() {
            String::new()
        } else {
            format!(":\n{}", tail.join("\n"))
        }
    }

    /// Run a container runtime command which writes `stdin` to the
    /// runtime. Its output gets inherited on debug log levels, otherwise the
    /// tail of its error output becomes part of the error. It stops on
    /// termination signals, since a build can take long.
    fn run_with_output(
        config: &Config,
        cmd: &mut Command,
        stdin: Option<&[u8]>,
        message: &str,
    ) -> Result<()> {
        let verbose = config.log_level() > LevelFilter::Info;
        cmd.stdout(Self::stdio(config)).stderr(if verbose {
            Stdio::inherit()
        } else {
            Stdio::piped()
        });
        let output = process::run_cancelable(cmd, stdin, signals::requested)
            .with_context(|| format!("Unable to run '{}'", config.container_runtime()))?;
        Self::check_output(&output, message)
    }

    /// Build the node image from the custom Dockerfile, using its directory
    /// as build context, which `exclude_args` exclude the run roots of.
    fn build_image(
        config: &Config,
        dockerfile: &Path,
        policy_json: &Path,
        image: &str,
        exclude_args: Vec<String>,
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
        let context = Self::build_context(dockerfile)?;
        args.extend(exclude_args);
        args.extend([
            format!("--file={}", dockerfile.display()),
            format!("--tag={image}"),
            context.display().to_string(),
        ]);

        debug!("Running container runtime with args: {}", args.join(" "));
        Self::run_with_output(
            config,
            process::command(config.container_runtime()).args(args),
            None,
            &format!(
                "Unable to build container image '{}' using '{}'",
                image,
                config.container_runtime()
            ),
        )?;
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
        Self::run_with_output(
            config,
            process::command(config.container_runtime()).args(args),
            Some(&Self::rootfs()),
            &format!(
                "Unable to import container image '{}' using '{}'",
                image,
                config.container_runtime()
            ),
        )
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

    /// Start a new container based process, with the additional environment
    /// variables `env`.
    pub fn start(
        config: &Config,
        dir: &Path,
        identifier: &str,
        process_name: &str,
        container_name: &str,
        args: &[&str],
        env: &[(&str, &str)],
    ) -> Result<Process> {
        let runtime_args = Self::runtime_args(config)?;
        let image = IMAGE
            .get()
            .context("The node container image has not been prepared")?;
        let name = Self::container_name(config, container_name);

        // Cleanup possible containers
        Self::remove(config, &runtime_args, &name);

        // Prepare the arguments
        let arg_hostname = &format!("--hostname={}", container_name);
        let arg_name = &format!("--name={}", name);
        let arg_label_root = &format!("--label={}={}", ROOT_LABEL, System::root_id(config.root()));
        let arg_label_path = &format!("--label={}={}", ROOT_PATH_LABEL, config.root().display());
        let arg_volume_root = &Self::volume_arg(config.root().display());
        let arg_volume_store = &format!("{}:ro", Self::volume_arg(NIX_STORE));
        let arg_entrypoint = &format!(
            "--entrypoint={}",
            System::find_executable(process_name)?.display()
        );
        let mut arg_env = Self::env_args();
        arg_env.extend(env.iter().map(|(k, v)| format!("--env={}={}", k, v)));
        let arg_ca_bundle = Self::ca_bundle_args();
        let arg_registry = Self::registry_args();
        let mut args_vec = vec![
            "run",
            // Never pull the node image, it only exists locally
            "--pull=never",
            "--net=host",
            "--privileged",
            "--rm",
            // An init process reaps the orphans of the node, like the
            // conmon-rs servers, whose exit CRI-O waits for
            "--init",
            "--cgroupns=host",
            arg_hostname,
            arg_name,
            arg_label_root,
            arg_label_path,
            arg_volume_root,
            arg_volume_store,
            arg_entrypoint,
        ];
        args_vec.extend(arg_env.iter().map(String::as_str));
        args_vec.extend(arg_ca_bundle.iter().map(String::as_str));
        args_vec.extend(arg_registry.iter().map(String::as_str));

        // The kubelet uses the resolver configuration of systemd-resolved
        // instead of its stub resolver. The directory gets mounted, since
        // the file gets replaced on changes.
        let resolv_conf = Network::resolv_conf();
        let arg_volume_resolv_conf = resolv_conf
            .parent()
            .filter(|_| resolv_conf == Path::new(SYSTEMD_RESOLV_CONF))
            .map(|dir| format!("{}:ro", Self::volume_arg(dir.display())));
        args_vec.extend(arg_volume_resolv_conf.as_deref());

        // Podman specific arguments
        args_vec.extend(runtime_args.iter().map(String::as_str));

        // Mount /dev/mapper if available (requires privileges)
        let dev_mapper = PathBuf::from("/dev/mapper");
        let arg_volume_dev_mapper = &Self::volume_arg(dev_mapper.display());
        if !config.is_rootless() && dev_mapper.exists() {
            args_vec.push(arg_volume_dev_mapper);
        }

        // Add the image and the user provided args
        args_vec.push(image);
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
            name: Self::container_name(config, container_name),
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
        Self::env_args_from(|key| var(key).ok())
    }

    /// The `PATH` and the set proxy variables. The proxy variables get
    /// passed by name, so the runtime takes their values from its
    /// environment and they do not show up in its arguments.
    fn env_args_from<F>(get: F) -> Vec<String>
    where
        F: Fn(&str) -> Option<String>,
    {
        get("PATH")
            .map(|path| format!("--env=PATH={}", path))
            .into_iter()
            .chain(
                PROXY_VARS
                    .iter()
                    .filter(|key| get(key).is_some())
                    .map(|key| format!("--env={}", key)),
            )
            .collect()
    }

    /// The arguments which mount the registry credentials and certificates
    /// of the host into the node containers, like CRI-O uses them on the
    /// host.
    fn registry_args() -> Vec<String> {
        Self::registry_args_from(|key| var(key).ok(), Path::exists)
    }

    /// Mount the existing credentials and certificate directories read-only,
    /// as well as the file of `REGISTRY_AUTH_FILE`, which gets forwarded.
    fn registry_args_from<F, E>(get: F, exists: E) -> Vec<String>
    where
        F: Fn(&str) -> Option<String>,
        E: Fn(&Path) -> bool,
    {
        let auth_file =
            get(REGISTRY_AUTH_FILE).filter(|f| Path::new(f).is_absolute() && exists(Path::new(f)));
        let mut args: Vec<String> = vec![];
        for path in auth_file
            .iter()
            .map(String::as_str)
            .chain(REGISTRY_AUTH_FILES)
            .chain(REGISTRY_CERT_DIRS)
            .filter(|path| exists(Path::new(path)))
        {
            let arg = format!("{}:ro", Self::volume_arg(path));
            if !args.contains(&arg) {
                args.push(arg);
            }
        }
        if auth_file.is_some() {
            args.push(format!("--env={}", REGISTRY_AUTH_FILE));
        }
        args
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

        let name = Self::container_name(config, container_name);
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
        Self::runtime_args_in(config, var_os("PATH").as_deref())
    }

    /// The arguments which apply to every container runtime command, with
    /// executables looked up in the provided `PATH`.
    fn runtime_args_in(config: &Config, path: Option<&OsStr>) -> Result<Vec<String>> {
        if Podman::is_configured_in(config, path) {
            Podman::default_args_in(config, path)
        } else {
            Ok(vec![])
        }
    }

    /// Remove the provided (maybe running) container.
    /// Failures are logged but not propagated since the container may not
    /// exist yet (first run).
    fn remove(config: &Config, runtime_args: &[String], name: &str) {
        Self::run_quietly(
            config.container_runtime(),
            runtime_args,
            &["rm", "-f", name],
        );
    }

    /// Run the container runtime, logging failures only, since they are
    /// expected, for example if a container does not exist.
    fn run_quietly(runtime: &str, runtime_args: &[String], args: &[&str]) {
        match process::command(runtime)
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
            CUSTOM_IMAGE_VERSION.to_vec(),
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

    /// The build arguments which exclude all run roots from the build
    /// context, the one of this run and every other directory which
    /// contains a `kubernix.toml`. The run roots contain the keys of the
    /// clusters, which must not end up in the image. Podman takes an
    /// ignore file, which extends the one of the build context. Other
    /// container runtimes refuse a build context with run roots.
    fn exclude_root_args(config: &Config, context: &Path) -> Result<Vec<String>> {
        let root = fs::canonicalize(config.root()).unwrap_or_else(|_| config.root().into());
        if context
            .ancestors()
            .any(|dir| dir == root || dir.join(Config::FILENAME).exists())
        {
            bail!(
                "The build context '{}' of the Dockerfile is part of a run directory, which \
                 would add the keys of the cluster to the image. Use a Dockerfile outside of it",
                context.display(),
            )
        }
        let roots = Self::context_roots(context, &root)?;
        if roots.is_empty() {
            return Ok(vec![]);
        }

        if !Podman::is_configured(config) {
            bail!(
                "The build context '{}' of the Dockerfile contains the run directories {}, \
                 which would add the keys of the clusters to the image. Move the Dockerfile \
                 into its own directory, or use podman as container runtime, which excludes \
                 them",
                context.display(),
                roots
                    .iter()
                    .map(|r| format!("'{}'", r.display()))
                    .collect::<Vec<_>>()
                    .join(", "),
            )
        }

        // The ignore file of the context does not apply with --ignorefile
        let mut content = [".containerignore", ".dockerignore"]
            .iter()
            .find_map(|file| fs::read_to_string(context.join(file)).ok())
            .unwrap_or_default();
        if !content.is_empty() && !content.ends_with('\n') {
            content.push('\n');
        }
        // The last matching pattern wins
        for root in &roots {
            let Some(pattern) = Self::ignore_pattern(root) else {
                bail!(
                    "The build context '{}' of the Dockerfile contains the run directory {:?}, \
                     which an ignore file cannot exclude, since its path is no valid UTF-8, \
                     starts or ends with whitespace or contains a line break. Move the \
                     Dockerfile into its own directory",
                    context.display(),
                    root,
                )
            };
            content.push_str(&pattern);
            content.push('\n');
        }
        let file = config.root().join("containerignore");
        write_file(&file, content.as_bytes())
            .context("Unable to write the container ignore file")?;
        Ok(vec![format!("--ignorefile={}", file.display())])
    }

    /// The run roots in the build context relative to it: the provided
    /// one, if it is part of it, and all directories which contain a
    /// `kubernix.toml`.
    fn context_roots(context: &Path, root: &Path) -> Result<Vec<PathBuf>> {
        let mut roots = vec![];
        Self::find_roots(context, context, root, &mut roots)?;
        if let Ok(relative) = root.strip_prefix(context)
            && !roots.iter().any(|r| relative.starts_with(r))
        {
            // The run root does not exist yet
            roots.push(relative.into());
        }
        Ok(roots)
    }

    /// Collect the run roots below `dir` recursively, see `context_roots`.
    fn find_roots(context: &Path, dir: &Path, root: &Path, roots: &mut Vec<PathBuf>) -> Result<()> {
        let mut entries = fs::read_dir(dir)
            .with_context(|| format!("Unable to read build context '{}'", dir.display()))?
            .collect::<Result<Vec<_>, _>>()
            .with_context(|| format!("Unable to read build context '{}'", dir.display()))?;
        entries.sort_by_key(|entry| entry.file_name());
        for entry in entries {
            let path = entry.path();
            // Symbolic links are not followed, like by the build
            let is_dir = entry
                .file_type()
                .with_context(|| format!("Unable to read '{}'", path.display()))?
                .is_dir();
            if path == root || (is_dir && path.join(Config::FILENAME).exists()) {
                roots.push(path.strip_prefix(context).unwrap_or(&path).into());
            } else if is_dir {
                Self::find_roots(context, &path, root, roots)?;
            }
        }
        Ok(())
    }

    /// The pattern of an ignore file which matches the relative path
    /// literally, if there is one. Podman and Docker trim the whitespace
    /// around patterns and read them line by line, and a leading `#` starts
    /// a comment unless it is escaped.
    fn ignore_pattern(path: &Path) -> Option<String> {
        let path = path.to_str()?;
        if path.trim() != path || path.contains(['\n', '\r']) {
            return None;
        }
        let mut pattern = String::new();
        for (i, c) in path.chars().enumerate() {
            if matches!(c, '*' | '?' | '[' | ']' | '\\' | '!') || (i == 0 && c == '#') {
                pattern.push('\\');
            }
            pattern.push(c);
        }
        Some(pattern)
    }

    /// Collect the metadata of all entries in the build context
    /// recursively, except the run root and all other directories which
    /// contain a `kubernix.toml`, since they are run roots as well. The
    /// metadata changes together with the content and is cheaper to read.
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
            if metadata.is_dir() && path.join(Config::FILENAME).exists() {
                // Another run root, whose content changes all the time
                continue;
            }
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
        process::command(config.container_runtime())
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
        let output = match process::command(config.container_runtime())
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

    /// The name of the node container, which is unique per run root.
    fn container_name(config: &Config, node: &str) -> String {
        format!(
            "{}-{}-{}",
            NAME_PREFIX,
            System::root_id(config.root()),
            node
        )
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
        let name = &self.name;
        debug!("Removing node container {}", name);
        Container::run_quietly(
            &self.runtime,
            &self.runtime_args,
            &["stop", "-t", "10", name],
        );
        Container::run_quietly(&self.runtime, &self.runtime_args, &["rm", "-f", name]);
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

    #[test]
    fn container_name_contains_root_id() -> Result<()> {
        let a = crate::config::tests::parse(&["--root=/a"])?;
        let b = crate::config::tests::parse(&["--root=/b"])?;
        assert_eq!(
            Container::container_name(&a, "node-0"),
            format!("kubernix-{}-node-0", System::root_id(Path::new("/a")))
        );
        assert_ne!(
            Container::container_name(&a, "node-0"),
            Container::container_name(&b, "node-0")
        );
        Ok(())
    }

    #[test]
    fn remove_stale_removes_labeled_containers() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let log = dir.path().join("log");
        let runtime = dir.path().join("fake-runtime");
        fs::write(
            &runtime,
            format!(
                "#!/bin/sh\n[ \"$1\" = --version ] && exit 0\necho \"$@\" >>{}\n[ \"$1\" = ps ] && printf 'a1\\nb2\\n'\nexit 0\n",
                log.display()
            ),
        )?;
        fs::set_permissions(
            &runtime,
            std::os::unix::fs::PermissionsExt::from_mode(0o755),
        )?;
        let c = crate::config::tests::parse(&[
            "--root=/some/root",
            "--nodes=2",
            "--container-runtime=fake-runtime",
        ])?;
        let path = format!("{}:/bin:/usr/bin", dir.path().display());
        Container::remove_stale(&c, Some(OsStr::new(&path)))?;
        assert_eq!(
            fs::read_to_string(&log)?,
            format!(
                "ps --all --quiet --filter=label=kubernix.root={}\nrm --force a1 b2\n",
                System::root_id(Path::new("/some/root"))
            )
        );
        assert!(Container::remove_stale(&c, Some(OsStr::new("/nonexistent"))).is_err());
        Ok(())
    }

    #[test]
    fn remove_legacy_removes_unlabeled_containers() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let log = dir.path().join("log");
        let runtime = dir.path().join("fake-runtime");
        fs::write(
            &runtime,
            format!(
                "#!/bin/sh\n[ \"$1\" = --version ] && exit 0\necho \"$@\" >>{}\n[ \"$1\" = ps ] && printf '{}'\nexit 0\n",
                log.display(),
                "kubernix-node-1\\nkubernix-0a1b2c3d-node-0\\nkubernix-node-0,other\\n",
            ),
        )?;
        fs::set_permissions(
            &runtime,
            std::os::unix::fs::PermissionsExt::from_mode(0o755),
        )?;
        let c = crate::config::tests::parse(&[
            "--root=/some/root",
            "--nodes=2",
            "--container-runtime=fake-runtime",
        ])?;
        let path = format!("{}:/bin:/usr/bin", dir.path().display());
        Container::remove_legacy(&c, Some(OsStr::new(&path)))?;
        assert_eq!(
            fs::read_to_string(&log)?,
            "ps --all --format={{.Names}}\nrm --force kubernix-node-0 kubernix-node-1\n"
        );
        assert!(Container::remove_legacy(&c, Some(OsStr::new("/nonexistent"))).is_err());

        // Only the running ones for the port conflict
        fs::remove_file(&log)?;
        assert_eq!(
            Container::legacy_containers(&c, Some(OsStr::new(&path)), false)?,
            ["kubernix-node-0", "kubernix-node-1"]
        );
        assert_eq!(fs::read_to_string(&log)?, "ps --format={{.Names}}\n");
        Ok(())
    }

    #[test]
    fn port_conflict_names_legacy_containers() -> Result<()> {
        let names = vec!["kubernix-node-0".to_string(), "kubernix-node-1".into()];
        let hint = Container::legacy_hint("podman", &names);
        assert!(
            hint.contains("(kubernix-node-0, kubernix-node-1)"),
            "{}",
            hint
        );
        assert!(
            hint.ends_with("remove them with: podman rm -f kubernix-node-0 kubernix-node-1"),
            "{}",
            hint
        );

        // Not for single node or rootless clusters
        let error = || anyhow::anyhow!("Port 6443 is already in use");
        let mut c = crate::config::tests::parse(&["--nodes=1"])?;
        let e = Container::explain_port_conflict(&c, error());
        assert_eq!(format!("{:#}", e), "Port 6443 is already in use");
        c = crate::config::tests::parse(&["--nodes=2"])?;
        c.set_rootless(true);
        let e = Container::explain_port_conflict(&c, error());
        assert_eq!(format!("{:#}", e), "Port 6443 is already in use");
        Ok(())
    }

    #[test]
    fn legacy_names_match_exactly() {
        assert!(Container::legacy_names("").is_empty());
        assert_eq!(
            Container::legacy_names(
                "kubernix-node-10\nkubernix-node-2,alias\nkubernix-node-2\n\
                 kubernix-0a1b2c3d-node-0\nkubernix-node-\nkubernix-node-x\n\
                 my-kubernix-node-3\nkubernix-node-4-old\n"
            ),
            vec!["kubernix-node-10", "kubernix-node-2"]
        );
    }

    #[test]
    fn stderr_tail_keeps_last_lines() {
        assert_eq!(Container::stderr_tail(b""), "");
        assert_eq!(Container::stderr_tail(b"\n  \n"), "");
        assert_eq!(Container::stderr_tail(b"a\n\nb  \n"), ":\na\nb");
        let long: String = (0..30).map(|i| format!("line {}\n", i)).collect();
        let tail = Container::stderr_tail(long.as_bytes());
        assert!(tail.starts_with(":\nline 10\n"));
        assert!(tail.ends_with("line 29"));
    }

    #[test]
    fn run_with_output_includes_stderr() -> Result<()> {
        let c = crate::config::tests::parse(&["--log-level=info"])?;
        let err = Container::run_with_output(
            &c,
            Command::new("sh").args([
                "-c",
                "cat >/dev/null; echo first >&2; echo last >&2; exit 3",
            ]),
            Some(b"input"),
            "Unable to run",
        )
        .unwrap_err();
        let err = format!("{:#}", err);
        assert!(
            err.starts_with("Unable to run (exit: exit status: 3):"),
            "{}",
            err
        );
        assert!(err.ends_with("first\nlast"), "{}", err);

        Container::run_with_output(&c, Command::new("sh").args(["-c", "cat"]), Some(b"x"), "")?;
        Container::run_with_output(&c, &mut Command::new("true"), None, "")?;
        Ok(())
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
            crate::config::tests::parse(&[
                "--nodes=2",
                &format!("--root={}", root.display()),
                &format!("--dockerfile={}", dockerfile.display()),
            ])
        };
        let image = Container::image(&config()?)?;
        assert!(image.starts_with("kubernix:base-"));

        // The images of older versions do not get used
        let mut inputs = vec![fs::read(&dockerfile)?];
        let canonical = fs::canonicalize(context.path())?;
        Container::context_inputs(&canonical, &canonical, &root, &mut inputs)?;
        let unversioned: Vec<&[u8]> = inputs.iter().map(Vec::as_slice).collect();
        assert_ne!(image, Container::image_name(&unversioned));
        let mut versioned = vec![CUSTOM_IMAGE_VERSION];
        versioned.extend(&unversioned);
        assert_eq!(image, Container::image_name(&versioned));

        assert_ne!(image, Container::image(&crate::config::tests::parse(&[])?)?);
        assert_eq!(image, Container::image(&config()?)?);

        // The run root and other run roots are excluded
        fs::write(root.join("state"), "changed")?;
        assert_eq!(image, Container::image(&config()?)?);
        let other = context.path().join("other-run");
        fs::create_dir(&other)?;
        assert_ne!(image, Container::image(&config()?)?);
        fs::write(other.join(Config::FILENAME), "")?;
        fs::write(other.join("kubernix.log"), "log")?;
        assert_eq!(image, Container::image(&config()?)?);
        fs::remove_dir_all(&other)?;

        // Files in the context and the Dockerfile are included
        fs::create_dir(context.path().join("dir"))?;
        fs::write(context.path().join("dir").join("nested"), "b")?;
        let nested = Container::image(&config()?)?;
        assert_ne!(image, nested);
        fs::write(context.path().join("dir").join("nested"), "bc")?;
        assert_ne!(nested, Container::image(&config()?)?);
        fs::write(&dockerfile, "FROM alpine\n")?;
        assert_ne!(nested, Container::image(&config()?)?);
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

    /// A canonical build context with a Dockerfile and a run root, which
    /// gets removed on drop.
    fn build_context(root: &str) -> Result<(tempfile::TempDir, PathBuf, PathBuf, PathBuf)> {
        let dir = tempfile::tempdir()?;
        let context = fs::canonicalize(dir.path())?;
        let dockerfile = context.join("Dockerfile");
        fs::write(&dockerfile, "FROM scratch\n")?;
        let root = context.join(root);
        fs::create_dir_all(&root)?;
        Ok((dir, context, dockerfile, root))
    }

    fn exclude_config(root: &Path, runtime: &str) -> Result<Config> {
        crate::config::tests::parse(&[
            "--nodes=2",
            "--root",
            &root.display().to_string(),
            "--container-runtime",
            runtime,
        ])
    }

    #[test]
    fn exclude_root_args_for_podman() -> Result<()> {
        let (_dir, context, _, root) = build_context("sub/run*")?;
        let config = exclude_config(&root, "podman")?;

        // Outside of the context
        let (_other, other, _, _) = build_context("unused")?;
        assert!(Container::exclude_root_args(&config, &other)?.is_empty());

        fs::write(context.join(".dockerignore"), "*.log\n!keep.log")?;
        let args = Container::exclude_root_args(&config, &context)?;
        let file = root.join("containerignore");
        assert_eq!(args, [format!("--ignorefile={}", file.display())]);
        assert_eq!(fs::read_to_string(&file)?, "*.log\n!keep.log\nsub/run\\*\n");

        // The containerignore file takes precedence
        fs::write(context.join(".containerignore"), "secret\n")?;
        Container::exclude_root_args(&config, &context)?;
        assert_eq!(fs::read_to_string(&file)?, "secret\nsub/run\\*\n");

        // Other run roots get excluded as well
        let another = context.join("a").join("other");
        fs::create_dir_all(another.join("nested"))?;
        fs::write(another.join(Config::FILENAME), "")?;
        fs::write(another.join("nested").join(Config::FILENAME), "")?;
        Container::exclude_root_args(&config, &context)?;
        assert_eq!(fs::read_to_string(&file)?, "secret\na/other\nsub/run\\*\n");

        // Run roots which an ignore file cannot exclude
        let space = context.join(" space");
        fs::create_dir(&space)?;
        fs::write(space.join(Config::FILENAME), "")?;
        let err = Container::exclude_root_args(&config, &context)
            .unwrap_err()
            .to_string();
        assert!(err.contains("run directory \" space\""), "{err}");
        fs::remove_dir_all(&space)?;

        // A context within a run root
        assert!(Container::exclude_root_args(&config, &root).is_err());
        assert!(Container::exclude_root_args(&config, &another.join("nested")).is_err());
        Ok(())
    }

    /// A container runtime other than podman, which does not depend on
    /// the host, where `docker` may be podman.
    const DOCKER: &str = "/nonexistent/docker";

    #[test]
    fn exclude_root_args_for_docker() -> Result<()> {
        let (_dir, context, _, root) = build_context("run")?;
        let config = exclude_config(&root, DOCKER)?;
        let exclude = || Container::exclude_root_args(&config, &context);
        let err = exclude().unwrap_err().to_string();
        assert!(err.contains("run directories 'run',"), "{err}");
        assert!(
            err.contains("Move the Dockerfile into its own directory"),
            "{err}"
        );

        // An ignore file does not help
        fs::write(context.join(".dockerignore"), "run\n")?;
        assert!(exclude().is_err());

        // Other run roots get refused as well
        fs::write(root.join(Config::FILENAME), "")?;
        let (_other, other, _, _) = build_context("unused")?;
        let other_config = exclude_config(&other.join("unused"), DOCKER)?;
        let another = context.join("x").join("other");
        fs::create_dir_all(&another)?;
        fs::write(another.join(Config::FILENAME), "")?;
        let err = Container::exclude_root_args(&other_config, &context)
            .unwrap_err()
            .to_string();
        assert!(err.contains("run directories 'run', 'x/other',"), "{err}");

        // A context without run roots
        fs::remove_dir_all(context.join("x"))?;
        fs::remove_dir_all(&root)?;
        assert!(Container::exclude_root_args(&other_config, &context)?.is_empty());
        Ok(())
    }

    #[test]
    fn build_checks_context_before_existing_image() -> Result<()> {
        let (_dir, context, dockerfile, root) = build_context("run")?;
        // A runtime for which every command succeeds, so the image exists
        let log = context.join("log");
        let runtime = context.join("fake-runtime");
        fs::write(
            &runtime,
            format!(
                "#!/bin/sh\n[ \"$1\" = --version ] && exit 0\necho \"$@\" >>{}\n",
                log.display()
            ),
        )?;
        fs::set_permissions(
            &runtime,
            std::os::unix::fs::PermissionsExt::from_mode(0o755),
        )?;
        let config = crate::config::tests::parse(&[
            "--nodes=2",
            &format!("--root={}", root.display()),
            &format!("--dockerfile={}", dockerfile.display()),
            &format!("--container-runtime={}", runtime.display()),
        ])?;
        let err = format!("{:#}", Container::build(&config).unwrap_err());
        assert!(err.contains("contains the run directories 'run'"), "{err}");
        let log = fs::read_to_string(&log).unwrap_or_default();
        assert!(!log.contains("inspect"), "{log}");
        Ok(())
    }

    #[test]
    fn ignore_pattern_escapes() {
        use std::os::unix::ffi::OsStrExt;
        let pattern = |path: &str| Container::ignore_pattern(Path::new(path));
        assert_eq!(
            pattern("a/!b[c]*?\\").as_deref(),
            Some("a/\\!b\\[c\\]\\*\\?\\\\")
        );
        assert_eq!(pattern("#a/#b").as_deref(), Some("\\#a/#b"));
        assert_eq!(pattern("a b").as_deref(), Some("a b"));
        assert_eq!(pattern(" a"), None);
        assert_eq!(pattern("a\t"), None);
        assert_eq!(pattern("a\nb"), None);
        assert_eq!(pattern("a\rb"), None);
        assert_eq!(
            Container::ignore_pattern(Path::new(OsStr::from_bytes(b"a\xff"))),
            None
        );
    }

    #[test]
    fn image_default_is_rootfs() -> Result<()> {
        let c = crate::config::tests::parse(&["--nodes=2"])?;
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

    fn lookup<'a>(vars: &'a [(&str, &str)]) -> impl Fn(&str) -> Option<String> + 'a {
        |key| {
            vars.iter()
                .find(|(k, _)| *k == key)
                .map(|(_, v)| v.to_string())
        }
    }

    #[test]
    fn env_args_forward_path_and_proxies() {
        assert!(Container::env_args_from(lookup(&[])).is_empty());
        assert_eq!(
            Container::env_args_from(lookup(&[("PATH", "/bin")])),
            vec!["--env=PATH=/bin"]
        );
        assert_eq!(
            Container::env_args_from(lookup(&[
                ("no_proxy", "localhost"),
                ("PATH", "/bin"),
                ("HTTPS_PROXY", "http://user:pass@proxy:3128"),
                ("FTP_PROXY", "http://proxy"),
            ])),
            vec!["--env=PATH=/bin", "--env=HTTPS_PROXY", "--env=no_proxy"]
        );
    }

    #[test]
    fn registry_args_mount_existing_paths() {
        let exists = |files: &'static [&'static str]| {
            move |p: &Path| files.iter().any(|f| Path::new(f) == p)
        };
        assert!(Container::registry_args_from(lookup(&[]), exists(&[])).is_empty());
        assert_eq!(
            Container::registry_args_from(
                lookup(&[]),
                exists(&[
                    "/root/.docker/config.json",
                    "/etc/containers/certs.d",
                    "/etc/containerd/certs.d",
                ])
            ),
            vec![
                "--volume=/root/.docker/config.json:/root/.docker/config.json:ro",
                "--volume=/etc/containers/certs.d:/etc/containers/certs.d:ro",
                "--volume=/etc/containerd/certs.d:/etc/containerd/certs.d:ro",
            ]
        );

        // A custom auth file gets forwarded, missing or relative ones not
        assert_eq!(
            Container::registry_args_from(
                lookup(&[(REGISTRY_AUTH_FILE, "/run/containers/0/auth.json")]),
                exists(&["/run/containers/0/auth.json"])
            ),
            vec![
                "--volume=/run/containers/0/auth.json:/run/containers/0/auth.json:ro",
                "--env=REGISTRY_AUTH_FILE",
            ]
        );
        assert!(
            Container::registry_args_from(
                lookup(&[(REGISTRY_AUTH_FILE, "/etc/missing.json")]),
                exists(&[])
            )
            .is_empty()
        );
        assert!(
            Container::registry_args_from(
                lookup(&[(REGISTRY_AUTH_FILE, "auth.json")]),
                exists(&["auth.json"])
            )
            .is_empty()
        );
    }

    fn ca_bundle_args(vars: &[(&str, &str)], files: &[(&str, &str)]) -> Vec<String> {
        Container::ca_bundle_args_from(lookup(vars), |path| {
            files
                .iter()
                .find(|(f, _)| Path::new(f) == path)
                .map(|(_, target)| PathBuf::from(target))
        })
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
        let c = crate::config::tests::test_config()?;
        assert!(Container::node(&c, "node-0").is_none());
        let mut c = crate::config::tests::parse(&["--nodes=2", "--container-runtime=true"])?;
        c.set_rootless(true);
        assert!(Container::node(&c, "node-0").is_none());
        c.set_rootless(false);
        let mut node = Container::node(&c, "node-0").context("missing node container")?;
        assert_eq!(node.name, Container::container_name(&c, "node-0"));
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
