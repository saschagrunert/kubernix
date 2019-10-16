use crate::{nix::Nix, podman::Podman, process::Process, system::System, Config};
use failure::{bail, Fallible};
use log::{info, trace, LevelFilter};
use std::{
    fs,
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

        // Prepare the arguments
        let mut args = if Podman::is_configured(config) {
            Podman::build_args(config, &policy_json)?
        } else {
            vec!["build".into()]
        };
        args.extend(vec![format!("-t={}", DEFAULT_IMAGE), ".".into()]);
        trace!("Container runtime build args: {:?}", args);

        // Run the build
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
        let podman_args = Podman::default_args(config);
        if Podman::is_configured(config) {
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
        trace!("Container runtime start args: {:?}", args_vec);
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
        let mut args_vec = vec![];

        let podman_args = Podman::default_args(config);
        if Podman::is_configured(config) {
            args_vec.extend(podman_args.iter().map(|x| x.as_str()).collect::<Vec<_>>())
        }

        let name = Self::prefixed_container_name(container_name);
        args_vec.extend(vec![
            "exec",
            &name,
            "nix",
            "run",
            "-f",
            DEFAULT_ROOT,
            "-c",
            process_name,
        ]);
        args_vec.extend(args);

        // Run as usual process
        trace!("Container runtime exec args: {:?}", args_vec);
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

    /// Retrieve a prefixed container name
    fn prefixed_container_name(name: &str) -> String {
        format!("{}-{}", DEFAULT_ROOT, name)
    }
}
