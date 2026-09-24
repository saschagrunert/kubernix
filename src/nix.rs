//! Nix environment bootstrapping.
//!
//! Manages the `nix develop` shell that provides all runtime
//! dependencies (etcd, kubernetes, CRI runtimes, etc.) at pinned
//! versions. Re-executes the kubernix binary inside the Nix shell,
//! forwarding all CLI options.

use crate::{Config, system::System};
use anyhow::{Context, Result, bail};
use log::{debug, info};
use std::{
    env::{current_exe, var},
    fs::{self, create_dir_all},
    path::Path,
    process::Command,
};

/// Handles Nix shell bootstrapping and command execution.
pub struct Nix;

impl Nix {
    pub const DIR: &'static str = "nix";

    /// Files of the generated Nix environment.
    pub const FILES: [&'static str; 4] = ["flake.nix", "flake.lock", "packages.nix", "overlay.nix"];
    const NIX_ENV: &'static str = "IN_NIX";

    /// Bootstrap the nix environment
    pub fn bootstrap(config: Config) -> Result<()> {
        // Prepare the nix dir
        debug!("Nix environment not found, bootstrapping one");
        let dir = config.root().join(Self::DIR);

        // Write the environment, regenerating it if the rendered files
        // differ, for example after upgrading kubernix or changing packages.
        if Self::write_files(&dir, &Self::render_files(&config)?)? {
            // Initialize a standalone git repo so that Nix flakes can
            // discover the generated files. Without this, Nix would use
            // the parent git worktree and filter to only tracked files.
            Self::git_init(&dir)?;
        }

        // Run the shell, forwarding all config options
        let exe = current_exe()?.display().to_string();
        let root = config.root().display().to_string();
        let log_level = config.log_level().to_string();
        let log_format = config.log_format().to_string();
        let cidr = config.cidr().to_string();
        let nodes = config.nodes().to_string();
        let container_runtime = config.container_runtime();
        let cri_runtime = config.cri_runtime().to_string();
        let oci_runtime = config.oci_runtime().to_string();

        let shell_val: String = config.shell().unwrap_or_default().to_owned();
        let overlay_val = config.overlay().map(|o| o.display().to_string());
        let mut args = vec![
            exe.as_str(),
            "--root",
            root.as_str(),
            "--log-level",
            log_level.as_str(),
            "--log-format",
            log_format.as_str(),
            "--cidr",
            cidr.as_str(),
            "--nodes",
            nodes.as_str(),
            "--container-runtime",
            container_runtime,
            "--cri-runtime",
            cri_runtime.as_str(),
            "--oci-runtime",
            oci_runtime.as_str(),
        ];

        if let Some(ref overlay) = overlay_val {
            args.push("--overlay");
            args.push(overlay.as_str());
        }

        let dockerfile_val = config.dockerfile().map(|d| d.display().to_string());
        if let Some(ref dockerfile) = dockerfile_val {
            args.push("--dockerfile");
            args.push(dockerfile.as_str());
        }

        for pkg in config.packages() {
            args.push("--packages");
            args.push(pkg.as_str());
        }

        for addon in config.addons() {
            args.push("--addons");
            args.push(addon.as_str());
        }

        if config.no_shell() {
            args.push("--no-shell");
        } else if !shell_val.is_empty() {
            args.push("--shell");
            args.push(&shell_val);
        }

        Self::run(&config, &args)
    }

    /// Run a command inside the nix develop shell.
    ///
    /// Each element in `args` is passed as a separate OS argument to
    /// `nix develop --command`, avoiding shell interpretation.
    pub fn run(config: &Config, args: &[&str]) -> Result<()> {
        let nix_dir = config.root().join(Self::DIR);
        let flake_ref = format!("path:{}", nix_dir.display());

        let mut cmd = Command::new(System::find_executable("nix")?);
        cmd.env(Nix::NIX_ENV, "true")
            .arg("develop")
            .arg(&flake_ref)
            .arg("--no-update-lock-file")
            .arg("--log-format")
            .arg("raw")
            .arg("--command");
        for arg in args {
            cmd.arg(arg);
        }
        let status = cmd.status().context("Unable to run nix develop")?;
        if !status.success() {
            bail!("nix develop exited with status {}", status);
        }
        Ok(())
    }

    /// Render all files of the Nix environment as (file name, content).
    fn render_files(config: &Config) -> Result<Vec<(&'static str, String)>> {
        for pkg in config.packages() {
            if !pkg
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_' || b == b'.')
            {
                bail!(
                    "Invalid package name '{}': only alphanumeric characters, dashes, underscores, and dots are allowed",
                    pkg,
                );
            }
        }
        debug!("Adding additional packages: {:?}", config.packages());

        let overlay = match config.overlay() {
            // User defined overlay
            Some(overlay) => {
                info!("Using custom overlay '{}'", overlay.display());
                fs::read_to_string(overlay).context("Unable to read custom overlay")?
            }

            // The default overlay
            None => {
                debug!("Using default overlay");
                include_str!("../nix/overlay.nix").into()
            }
        };

        let [flake, lock, packages, overlay_file] = Self::FILES;
        Ok(vec![
            (
                flake,
                include_str!("../nix/runtime-flake.nix")
                    .replace("KUBERNIX_SYSTEM", Self::nix_system()?),
            ),
            (lock, include_str!("../flake.lock").into()),
            (
                packages,
                include_str!("../nix/packages.nix")
                    .replace("/* PACKAGES */", &config.packages().join(" ")),
            ),
            (overlay_file, overlay),
        ])
    }

    /// Write the files into the directory if the directory is new or any
    /// content differs. Returns true if something has been written.
    fn write_files(dir: &Path, files: &[(&str, String)]) -> Result<bool> {
        let exists = dir.exists();
        let changed = files.iter().any(|(name, content)| {
            fs::read_to_string(dir.join(name)).ok().as_ref() != Some(content)
        });
        if exists && !changed {
            return Ok(false);
        }
        if exists {
            info!("Nix environment is outdated, regenerating it");
        }
        create_dir_all(dir).context("Unable to create nix directory")?;
        for (name, content) in files {
            fs::write(dir.join(name), content)
                .with_context(|| format!("Unable to write {}", name))?;
        }
        Ok(true)
    }

    /// Initialize a git repo in the given directory so Nix flakes
    /// treats it as a standalone source tree.
    fn git_init(dir: &Path) -> Result<()> {
        let git = System::find_executable("git")?;
        let run = |args: &[&str]| -> Result<()> {
            let status = Command::new(&git).arg("-C").arg(dir).args(args).status()?;
            if !status.success() {
                bail!("git {} failed with status {}", args.join(" "), status,);
            }
            Ok(())
        };
        run(&["init", "-q"])?;
        run(&["config", "user.email", "kubernix@localhost"])?;
        run(&["config", "user.name", "kubernix"])?;
        run(&["config", "commit.gpgsign", "false"])?;
        run(&["add", "."])?;
        run(&["commit", "-q", "-m", "init", "--allow-empty"])?;
        Ok(())
    }

    /// Returns the Nix system string for the current architecture.
    fn nix_system() -> Result<&'static str> {
        match std::env::consts::ARCH {
            "x86_64" => Ok("x86_64-linux"),
            "aarch64" => Ok("aarch64-linux"),
            arch => bail!(
                "unsupported architecture '{}' (only x86_64 and aarch64 Linux are supported)",
                arch,
            ),
        }
    }

    /// Returns true if running in nix environment
    pub fn is_active() -> bool {
        var(Nix::NIX_ENV).is_ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn write_files_only_on_change() -> Result<()> {
        let dir = tempdir()?.keep().join("nix");
        let files = vec![("a.nix", "a".to_string()), ("b.nix", "b".to_string())];
        assert!(Nix::write_files(&dir, &files)?);
        assert!(!Nix::write_files(&dir, &files)?);

        let changed = vec![("a.nix", "a".to_string()), ("b.nix", "c".to_string())];
        assert!(Nix::write_files(&dir, &changed)?);
        assert_eq!(fs::read_to_string(dir.join("b.nix"))?, "c");

        fs::remove_file(dir.join("a.nix"))?;
        assert!(Nix::write_files(&dir, &changed)?);
        assert!(dir.join("a.nix").exists());
        Ok(())
    }

    #[test]
    fn render_files_contains_runc() -> Result<()> {
        let config = crate::config::tests::test_config()?;
        let files = Nix::render_files(&config)?;
        let packages = &files.iter().find(|(n, _)| *n == "packages.nix").unwrap().1;
        assert!(packages.contains("runc"));
        assert!(packages.contains("crun"));
        Ok(())
    }

    #[test]
    fn nix_system_success() {
        let system = Nix::nix_system().unwrap();
        assert!(
            system == "x86_64-linux" || system == "aarch64-linux",
            "unexpected system: {}",
            system,
        );
        assert!(system.ends_with("-linux"));
    }

    #[test]
    fn git_init_success() {
        let dir = tempdir().unwrap().keep();
        std::fs::write(dir.join("test.txt"), "hello").unwrap();
        Nix::git_init(&dir).unwrap();
        assert!(dir.join(".git").exists());
    }

    #[test]
    fn git_init_invalid_dir() {
        let dir = Path::new("/nonexistent/path");
        assert!(Nix::git_init(dir).is_err());
    }

    #[test]
    fn runtime_flake_template_contains_placeholder() {
        let template = include_str!("../nix/runtime-flake.nix");
        assert!(template.contains("KUBERNIX_SYSTEM"));
    }

    #[test]
    fn runtime_flake_system_replacement() {
        let template = include_str!("../nix/runtime-flake.nix");
        let result = template.replace("KUBERNIX_SYSTEM", "x86_64-linux");
        assert!(result.contains("x86_64-linux"));
        assert!(!result.contains("KUBERNIX_SYSTEM"));
    }

    #[test]
    fn packages_template_contains_placeholder() {
        let template = include_str!("../nix/packages.nix");
        assert!(template.contains("/* PACKAGES */"));
    }

    #[test]
    fn packages_replacement() {
        let template = include_str!("../nix/packages.nix");
        let result = template.replace("/* PACKAGES */", "hello world");
        assert!(result.contains("hello world"));
        assert!(!result.contains("/* PACKAGES */"));
    }

    #[test]
    fn flake_lock_is_valid_json() {
        let lock = include_str!("../flake.lock");
        let parsed: serde_json::Value = serde_json::from_str(lock).unwrap();
        assert!(parsed["nodes"]["nixpkgs"]["locked"]["rev"].is_string());
    }

    #[test]
    fn is_active_toggle() {
        temp_env::with_var(Nix::NIX_ENV, None::<&str>, || {
            assert!(!Nix::is_active());
        });
        temp_env::with_var(Nix::NIX_ENV, Some("true"), || {
            assert!(Nix::is_active());
        });
    }
}
