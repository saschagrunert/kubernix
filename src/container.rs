use crate::{nix::Nix, process::Process, system::System, Config, PODMAN};
use failure::{bail, Fallible};
use log::{info, trace, LevelFilter};
use std::{
    fs::{self, create_dir_all},
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

const DEFAULT_IMAGE: &str = "kubernix:base";
const DEFAULT_ROOT: &str = "kubernix";

pub struct Container;

impl Container {
    /// Build the base image used for the nodes
    pub fn build(config: &Config) -> Fallible<()> {
        // Verify that the provided runtime exists
        System::find_executable(config.container_runtime())?;

        // Write the policy file
        let policy_json = Self::policy_json(config);
        fs::write(&policy_json, include_str!("assets/policy.json"))?;

        // Nothing needs to be done on single node runs
        if config.nodes() <= 1 {
            return Ok(());
        }

        // Build the base container image
        info!("Building base container image '{}'", DEFAULT_IMAGE);

        // Prepare the Dockerfile
        let file = config.root().join("Dockerfile");
        if !file.exists() {
            fs::write(
                &file,
                format!(
                    include_str!("assets/Dockerfile"),
                    nix = Nix::DIR,
                    root = DEFAULT_ROOT
                ),
            )?;
        }

        let mut args = vec![];
        if config.container_runtime() == PODMAN {
            // Prepare podman CNI
            let dir = Self::podman_cni_dir(config);
            create_dir_all(&dir)?;
            fs::write(
                &dir.join("87-podman-bridge.conflist"),
                include_str!("assets/podman-bridge.json"),
            )?;

            // Add podman specific arguments
            args.extend(Self::default_podman_args(config));
            args.extend(vec![
                "build".into(),
                format!("--signature-policy={}", policy_json.display()),
            ]);
        } else {
            args.push("build".into());
        }

        // Run the build
        args.extend(vec![format!("-t={}", DEFAULT_IMAGE), ".".into()]);
        trace!("Podman build args: {:?}", args);
        let status = Command::new(config.container_runtime())
            .current_dir(config.root())
            .args(args)
            .stderr(Self::stdio(config))
            .stdout(Self::stdio(config))
            .status()?;
        if !status.success() {
            bail!("Unable to build container base image");
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
    ) -> Fallible<Process> {
        // Cleanup possible containers
        Self::remove(config, container_name)?;

        // Prepare the arguments
        let arg_hostname = &format!("--hostname={}", container_name);
        let arg_name = &format!("--name={}", Self::prefixed_container_name(container_name));
        let arg_volume_root = &format!("--volume={v}:{v}", v = config.root().display());
        let mut args_vec = vec![
            "run",
            "--rm",
            "--net=host",
            "--privileged",
            arg_hostname,
            arg_name,
            arg_volume_root,
        ];

        // Podman specific arguments
        let podman_args = Self::default_podman_args(config);
        if config.container_runtime() == PODMAN {
            args_vec.extend(podman_args.iter().map(|x| x.as_str()).collect::<Vec<_>>())
        }

        // Mount /dev/mapper if available
        let dev_mapper = PathBuf::from("/").join("dev").join("mapper");
        let arg_volume_dev_mapper = &format!("--volume={v}:{v}", v = dev_mapper.display());
        if dev_mapper.exists() {
            args_vec.push(arg_volume_dev_mapper);
        }

        // Add the process and the user provided args
        args_vec.extend(&[DEFAULT_IMAGE, process_name]);
        args_vec.extend(args);

        // Start the process
        trace!("Podman start args: {:?}", args_vec);
        Process::start(dir, identifier, config.container_runtime(), &args_vec)
    }

    /// Exec a command on a container instance
    pub fn exec(
        config: &Config,
        dir: &Path,
        identifier: &str,
        process_name: &str,
        container_name: &str,
        args: &[&str],
    ) -> Fallible<Process> {
        // Prepare the args
        let n = Self::prefixed_container_name(container_name);
        let a = Self::default_podman_args(config);
        let mut args_vec = a.iter().map(|x| x.as_str()).collect::<Vec<_>>();
        args_vec.extend(vec![
            "exec",
            &n,
            "nix",
            "run",
            "-f",
            DEFAULT_ROOT,
            "-c",
            process_name,
        ]);
        args_vec.extend(args);

        // Run as usual process
        trace!("Podman exec args: {:?}", args_vec);
        Process::start(dir, identifier, config.container_runtime(), &args_vec)
    }

    /// Remove the provided (maybe running) container
    fn remove(config: &Config, name: &str) -> Fallible<()> {
        Command::new(config.container_runtime())
            .arg("rm")
            .arg("-f")
            .arg(Self::prefixed_container_name(name))
            .stderr(Stdio::null())
            .stdout(Stdio::null())
            .status()?;
        Ok(())
    }

    /// Retrieve a stdio for the provided config log level
    fn stdio(config: &Config) -> Stdio {
        if config.log_level() > LevelFilter::Info {
            Stdio::inherit()
        } else {
            Stdio::null()
        }
    }

    /// Podman args which should apply to every command
    fn default_podman_args(config: &Config) -> Vec<String> {
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
            format!(
                "--cni-config-dir={}",
                Self::podman_cni_dir(config).display()
            ),
            "--events-backend=none".into(),
            "--cgroup-manager=cgroupfs".into(),
        ]
    }

    /// Retrieve a prefixed container name
    fn prefixed_container_name(name: &str) -> String {
        format!("{}-{}", DEFAULT_ROOT, name)
    }

    fn podman_cni_dir(config: &Config) -> PathBuf {
        config.root().join("podman")
    }
}
