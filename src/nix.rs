//! Nix environment bootstrapping.
//!
//! Manages the `nix develop` shell that provides all runtime
//! dependencies (etcd, kubernetes, CRI-O, etc.) at pinned versions.
//! Re-executes the kubernix binary inside the Nix shell, forwarding
//! all CLI options.

use crate::{Config, system::System};
use anyhow::{Result, bail};
use log::{debug, info};
use std::{
    env::{current_exe, var},
    fs::{self, create_dir_all},
    process::Command,
};

/// Handles Nix shell bootstrapping and command execution.
pub struct Nix;

impl Nix {
    pub const DIR: &'static str = "nix";
    const NIX_ENV: &'static str = "IN_NIX";

    /// Bootstrap the nix environment
    pub fn bootstrap(config: Config) -> Result<()> {
        // Prepare the nix dir
        debug!("Nix environment not found, bootstrapping one");
        let dir = config.root().join(Self::DIR);

        // Write the configuration if not existing
        if !dir.exists() {
            create_dir_all(&dir)?;

            fs::write(
                dir.join("flake.nix"),
                include_str!("../nix/runtime-flake.nix")
                    .replace("KUBERNIX_SYSTEM", Self::nix_system()?),
            )?;
            fs::write(dir.join("flake.lock"), include_str!("../flake.lock"))?;

            let packages = &config.packages().join(" ");
            debug!("Adding additional packages: {:?}", config.packages());
            fs::write(
                dir.join("packages.nix"),
                include_str!("../nix/packages.nix").replace("/* PACKAGES */", packages),
            )?;

            // Apply the overlay if existing
            let target_overlay = dir.join("overlay.nix");
            match config.overlay() {
                // User defined overlay
                Some(overlay) => {
                    info!("Using custom overlay '{}'", overlay.display());
                    fs::copy(overlay, target_overlay)?;
                }

                // The default overlay
                None => {
                    debug!("Using default overlay");
                    fs::write(target_overlay, include_str!("../nix/overlay.nix"))?;
                }
            }

            // Initialize a standalone git repo so that Nix flakes can
            // discover the generated files. Without this, Nix would use
            // the parent git worktree and filter to only tracked files.
            Self::git_init(&dir)?;
        }

        // Run the shell, forwarding all config options
        let exe = format!("{}", current_exe()?.display());
        let root = format!("{}", config.root().display());
        let log_level = format!("{}", config.log_level());
        let log_format = format!("{}", config.log_format());
        let cidr = format!("{}", config.cidr());
        let nodes = format!("{}", config.nodes());
        let container_runtime = config.container_runtime();

        let shell_val: String = config.shell().unwrap_or_default().to_owned();
        let overlay_val = config.overlay().map(|o| format!("{}", o.display()));
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
        ];

        if let Some(ref overlay) = overlay_val {
            args.push("--overlay");
            args.push(overlay.as_str());
        }

        let dockerfile_val = config.dockerfile().map(|d| format!("{}", d.display()));
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

    /// Run a command inside the nix develop shell
    pub fn run(config: &Config, args: &[&str]) -> Result<()> {
        let nix_dir = config.root().join(Self::DIR);
        let flake_ref = format!("path:{}", nix_dir.display());

        let status = Command::new(System::find_executable("nix")?)
            .env(Nix::NIX_ENV, "true")
            .arg("develop")
            .arg(&flake_ref)
            .arg("--no-update-lock-file")
            .arg("--command")
            .arg("bash")
            .arg("-c")
            .arg(args.join(" "))
            .status()?;
        if !status.success() {
            bail!("nix develop exited with status {}", status);
        }
        Ok(())
    }

    /// Initialize a git repo in the given directory so Nix flakes
    /// treats it as a standalone source tree.
    fn git_init(dir: &std::path::Path) -> Result<()> {
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

    /// Set the NIX_ENV marker so re-exec knows we are inside Nix.
    ///
    /// # Safety
    /// Callers must ensure no other threads are concurrently reading
    /// or writing environment variables.
    #[cfg(test)]
    pub fn set_env_marker() {
        unsafe { std::env::set_var(Self::NIX_ENV, "true") };
    }

    /// Remove the NIX_ENV marker.
    ///
    /// # Safety
    /// Callers must ensure no other threads are concurrently reading
    /// or writing environment variables.
    #[cfg(test)]
    pub fn remove_env_marker() {
        unsafe { std::env::remove_var(Self::NIX_ENV) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
    use tempfile::tempdir;

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
    fn is_active_false_by_default() {
        // IN_NIX should not be set during tests
        Nix::remove_env_marker();
        assert!(!Nix::is_active());
    }

    #[test]
    fn is_active_true_when_set() {
        Nix::set_env_marker();
        assert!(Nix::is_active());
        Nix::remove_env_marker();
    }
}
