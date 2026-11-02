//! Podman container runtime integration.
//!
//! Provides podman-specific CLI arguments for image builds and container
//! execution, including the OCI runtime and storage driver configuration.
//! Podman finds conmon in its own helper binaries directory.

use crate::{Config, process, system::System};
use anyhow::Result;
use log::{LevelFilter, debug};
use std::{
    collections::HashMap,
    env::var_os,
    ffi::OsStr,
    fs,
    path::{Path, PathBuf},
    process::Stdio,
    sync::{Mutex, PoisonError},
};

/// Podman-specific argument builder for container operations.
pub struct Podman;

impl Podman {
    /// The executable name
    pub const EXECUTABLE: &'static str = "podman";

    /// Returns true if podman is configured as container runtime
    pub fn is_configured(config: &Config) -> bool {
        Self::is_configured_in(config, var_os("PATH").as_deref())
    }

    /// Returns true if podman is configured as container runtime, looking
    /// it up in the provided `PATH`. Besides its name, the executable gets
    /// checked after resolving symbolic links, and its version output, which
    /// covers shims like the `docker` command of podman-docker.
    pub fn is_configured_in(config: &Config, path: Option<&OsStr>) -> bool {
        let runtime = config.container_runtime();
        if Self::is_podman_name(Path::new(runtime)) {
            return true;
        }
        let executable = if runtime.contains('/') {
            PathBuf::from(runtime)
        } else {
            match System::find_executable_in(runtime, path) {
                Ok(executable) => executable,
                Err(_) => return false,
            }
        };
        let Ok(resolved) = fs::canonicalize(&executable) else {
            return false;
        };
        if Self::is_podman_name(&resolved) {
            return true;
        }

        // Running the version command is cheap, but happens for every
        // container runtime command otherwise
        static VERSIONS: Mutex<Option<HashMap<PathBuf, bool>>> = Mutex::new(None);
        let mut versions = VERSIONS.lock().unwrap_or_else(PoisonError::into_inner);
        *versions
            .get_or_insert_with(HashMap::new)
            .entry(resolved)
            .or_insert_with(|| Self::reports_podman(&executable))
    }

    /// Whether the file name of the executable is the one of podman.
    fn is_podman_name(executable: &Path) -> bool {
        executable.file_name() == Some(OsStr::new(Self::EXECUTABLE))
    }

    /// Whether the version output of the executable is the one of podman.
    fn reports_podman(executable: &Path) -> bool {
        match process::command(executable)
            .arg("--version")
            .stderr(Stdio::null())
            .output()
        {
            Ok(output) => output.status.success() && output.stdout.starts_with(b"podman version"),
            Err(e) => {
                debug!("Unable to run '{} --version': {}", executable.display(), e);
                false
            }
        }
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
        Self::default_args_in(config, var_os("PATH").as_deref())
    }

    /// Podman args which should apply to every command, looking up crun
    /// in the provided `PATH`.
    pub fn default_args_in(config: &Config, path: Option<&OsStr>) -> Result<Vec<String>> {
        Ok(Self::args(
            config.log_level(),
            &System::find_executable_in("crun", path)?,
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
    use crate::config::tests::parse;
    use std::path::PathBuf;

    #[test]
    fn is_configured_success() -> Result<()> {
        let c = parse(&["--container-runtime=podman"])?;
        assert!(Podman::is_configured_in(&c, None));
        let c = parse(&["--container-runtime=/usr/bin/podman"])?;
        assert!(Podman::is_configured_in(&c, None));
        let c = parse(&["--container-runtime=docker"])?;
        assert!(!Podman::is_configured_in(&c, None));
        Ok(())
    }

    #[test]
    fn is_configured_resolves_executable() -> Result<()> {
        use std::os::unix::fs::{PermissionsExt, symlink};
        let dir = tempfile::tempdir()?;
        let script = |name: &str, version: &str| -> Result<()> {
            let file = dir.path().join(name);
            fs::write(&file, format!("#!/bin/sh\necho '{version}'\n"))?;
            fs::set_permissions(&file, fs::Permissions::from_mode(0o755))?;
            // Another test thread may fork while the script is open for
            // writing, which makes running it fail until that child execs.
            for _ in 0..50 {
                match std::process::Command::new(&file).output() {
                    Err(e) if e.raw_os_error() == Some(nix::libc::ETXTBSY) => {
                        std::thread::sleep(std::time::Duration::from_millis(20))
                    }
                    result => {
                        result?;
                        break;
                    }
                }
            }
            Ok(())
        };
        script("podman", "not podman")?;
        script("shim", "podman version 5.0.0")?;
        script("docker", "Docker version 27.0.0")?;
        symlink(dir.path().join("podman"), dir.path().join("link"))?;
        let path = dir.path().as_os_str();
        let configured = |runtime: &str| -> Result<bool> {
            let c = parse(&[&format!("--container-runtime={runtime}")])?;
            Ok(Podman::is_configured_in(&c, Some(path)))
        };

        // A symbolic link to podman, by name or by path
        assert!(configured("link")?);
        assert!(configured(&dir.path().join("link").display().to_string())?);

        // A shim which runs podman, also when cached
        for _ in 0..2 {
            assert!(configured("shim")?);
            assert!(!configured("docker")?);
        }
        assert!(!configured("missing")?);
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
