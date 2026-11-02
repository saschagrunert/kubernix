//! Nix environment bootstrapping.
//!
//! Manages the `nix develop` shell that provides all runtime
//! dependencies (etcd, kubernetes, CRI runtimes, etc.) at pinned
//! versions. Re-executes the kubernix binary inside the Nix shell,
//! forwarding all CLI options. The evaluated environment gets cached in a
//! Nix profile, which avoids evaluating the flake on every start and keeps
//! its store paths from being garbage collected.

use crate::{Config, config::Addon, content_hash, system::System};
use ::nix::fcntl::{Flock, FlockArg};
use anyhow::{Context, Result, bail};
use log::{debug, info};
use std::{
    env::{current_exe, var},
    fs::{self, File, OpenOptions, create_dir_all},
    os::unix::process::CommandExt,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

/// Handles Nix shell bootstrapping and command execution.
pub struct Nix;

impl Nix {
    /// Directory of the generated Nix environment inside the run root.
    pub const DIR: &'static str = "nix";

    /// Files of the generated Nix environment.
    pub const FILES: [&'static str; 4] = ["flake.nix", "flake.lock", "packages.nix", "overlay.nix"];
    const NIX_ENV: &'static str = "IN_NIX";

    /// The directory of the cached environment. It cannot be part of the
    /// Nix directory, since the flake consists of all files in there.
    const PROFILE_DIR: &'static str = "nix-profile";

    /// The lock file which serializes the creation of the cached
    /// environment.
    const PROFILE_LOCK: &'static str = "nix-profile.lock";

    /// The `.dockerignore` content of older versions.
    const DOCKERIGNORE: &'static str = "nix/.git\n";

    /// Bootstrap the nix environment
    pub fn bootstrap(config: Config) -> Result<()> {
        // Prepare the nix dir
        debug!("Nix environment not found, bootstrapping one");
        let dir = config.root().join(Self::DIR);

        // Write the environment, regenerating it if the rendered files
        // differ, for example after upgrading kubernix or changing packages.
        // The `path:` flake reference makes Nix use all files of the
        // directory, even if it is part of a git worktree.
        if Self::write_files(&dir, &Self::render_files(&config)?)? {
            debug!("Nix environment written to '{}'", dir.display());
        }
        Self::remove_leftovers(config.root())?;

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

        let addons = Self::addons_arg(config.addons());
        args.push("--addons");
        args.push(addons.as_str());

        if config.no_shell() {
            args.push("--no-shell");
        } else if !shell_val.is_empty() {
            args.push("--shell");
            args.push(&shell_val);
        }

        Self::exec(&config, &args)
    }

    /// The `--addons` value for the addons, `none` explicitly disables the
    /// default addons.
    fn addons_arg(addons: &[Addon]) -> String {
        if addons.is_empty() {
            return Addon::None.to_string();
        }
        addons
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(",")
    }

    /// Replace the current process by a command inside the nix develop
    /// shell, which only returns on failure. Signals reach the command
    /// directly, since `nix develop` execs it as well.
    ///
    /// Each element in `args` is passed as a separate OS argument to
    /// `nix develop --command`, avoiding shell interpretation.
    pub fn exec(config: &Config, args: &[&str]) -> Result<()> {
        let nix = System::find_executable("nix")?;
        let profile = Self::profile(config, &nix)?;

        let mut cmd = Command::new(&nix);
        cmd.env(Nix::NIX_ENV, "true")
            .arg("develop")
            .arg(&profile)
            .arg("--log-format")
            .arg("raw")
            .arg("--command");
        for arg in args {
            cmd.arg(arg);
        }
        Err(cmd.exec()).context("Unable to run nix develop")
    }

    /// The cached environment of the Nix directory, which gets created if it
    /// does not exist or is unusable. Its path contains a hash of the
    /// environment files, so a changed environment results in a new one.
    /// The profile is a garbage collector root for the store paths of the
    /// environment.
    fn profile(config: &Config, nix: &Path) -> Result<PathBuf> {
        let profile = Self::profile_path(config.root())?;
        if Self::is_usable(&profile) {
            debug!("Using cached Nix environment '{}'", profile.display());
            return Ok(profile);
        }

        // A new shell may race with the start of the cluster. The lock of
        // the run directory cannot be used for that, since the running
        // cluster holds it.
        let _lock = Self::lock_profile(config.root())?;
        if Self::is_usable(&profile) {
            return Ok(profile);
        }
        if profile.symlink_metadata().is_ok() {
            info!("Cached Nix environment is unusable, recreating it");
        }

        // Remove the profiles of previous environments, which allows the
        // garbage collector to remove their store paths.
        let dir = config.root().join(Self::PROFILE_DIR);
        if dir.exists() {
            fs::remove_dir_all(&dir)
                .with_context(|| format!("Unable to remove '{}'", dir.display()))?;
        }
        create_dir_all(&dir).with_context(|| format!("Unable to create '{}'", dir.display()))?;

        info!("Evaluating the Nix environment");
        let nix_dir = config.root().join(Self::DIR);
        let status = Command::new(nix)
            .arg("print-dev-env")
            .arg("--profile")
            .arg(&profile)
            .arg(format!("path:{}", nix_dir.display()))
            .arg("--no-update-lock-file")
            .arg("--log-format")
            .arg("raw")
            .stdout(Stdio::null())
            .status()
            .context("Unable to run nix print-dev-env")?;
        if !status.success() {
            bail!("Unable to evaluate the Nix environment (exit: {})", status);
        }
        if !Self::is_usable(&profile) {
            bail!(
                "Created Nix environment '{}' is unusable",
                profile.display()
            );
        }
        Ok(profile)
    }

    /// Check whether `nix develop` is able to enter the provided profile:
    /// it has to point to the JSON of a development environment, whose
    /// `PATH` entries in the Nix store exist.
    fn is_usable(profile: &Path) -> bool {
        let Ok(content) = fs::read(profile) else {
            return false;
        };
        let Ok(env) = serde_json::from_slice::<serde_json::Value>(&content) else {
            return false;
        };
        env["variables"]["PATH"]["value"]
            .as_str()
            .is_some_and(|path| {
                path.split(':')
                    .filter(|dir| dir.starts_with("/nix/store/"))
                    .all(|dir| Path::new(dir).exists())
            })
    }

    /// Take the exclusive lock for creating the cached environment, which
    /// waits for other instances creating it.
    fn lock_profile(root: &Path) -> Result<Flock<File>> {
        let path = root.join(Self::PROFILE_LOCK);
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&path)
            .with_context(|| format!("Unable to open lock file '{}'", path.display()))?;
        Flock::lock(file, FlockArg::LockExclusive)
            .map_err(|(_, e)| e)
            .with_context(|| format!("Unable to lock '{}'", path.display()))
    }

    /// The path of the cached environment for the current files of the Nix
    /// directory.
    fn profile_path(root: &Path) -> Result<PathBuf> {
        let nix_dir = root.join(Self::DIR);
        let mut inputs = vec![];
        for file in Self::FILES {
            let path = nix_dir.join(file);
            inputs.push(
                fs::read(&path).with_context(|| format!("Unable to read '{}'", path.display()))?,
            );
        }
        let inputs: Vec<&[u8]> = inputs.iter().map(Vec::as_slice).collect();
        Ok(root
            .join(Self::PROFILE_DIR)
            .join(format!("{:016x}", content_hash(&inputs))))
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

    /// Remove the git repository of the Nix environment and its
    /// `.dockerignore`, which older versions created.
    fn remove_leftovers(root: &Path) -> Result<()> {
        let git = root.join(Self::DIR).join(".git");
        if git.exists() {
            debug!("Removing '{}'", git.display());
            fs::remove_dir_all(&git)
                .with_context(|| format!("Unable to remove '{}'", git.display()))?;
        }
        let dockerignore = root.join(".dockerignore");
        if fs::read_to_string(&dockerignore).is_ok_and(|c| c == Self::DOCKERIGNORE) {
            debug!("Removing '{}'", dockerignore.display());
            fs::remove_file(&dockerignore)
                .with_context(|| format!("Unable to remove '{}'", dockerignore.display()))?;
        }
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
    fn addons_arg() {
        assert_eq!(Nix::addons_arg(&[]), "none");
        assert_eq!(Nix::addons_arg(&[Addon::Coredns]), "coredns");
    }

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
    fn profile_path_depends_on_files() -> Result<()> {
        let root = tempdir()?;
        let dir = root.path().join(Nix::DIR);
        let files: Vec<(&str, String)> = Nix::FILES
            .iter()
            .map(|name| (*name, format!("{} content", name)))
            .collect();
        Nix::write_files(&dir, &files)?;
        let profile = Nix::profile_path(root.path())?;
        assert!(profile.starts_with(root.path().join(Nix::PROFILE_DIR)));
        assert_eq!(profile, Nix::profile_path(root.path())?);

        fs::write(dir.join("overlay.nix"), "changed")?;
        assert_ne!(profile, Nix::profile_path(root.path())?);

        fs::remove_file(dir.join("flake.lock"))?;
        assert!(Nix::profile_path(root.path()).is_err());
        Ok(())
    }

    #[test]
    fn is_usable_checks_environment() -> Result<()> {
        let dir = tempdir()?;
        let profile = dir.path().join("profile");
        assert!(!Nix::is_usable(&profile));

        fs::write(&profile, "no json")?;
        assert!(!Nix::is_usable(&profile));

        fs::write(&profile, r#"{"variables": {}}"#)?;
        assert!(!Nix::is_usable(&profile));

        let env = |path: &str| {
            format!(
                r#"{{"variables": {{"PATH": {{"type": "exported", "value": "{}"}}}}}}"#,
                path
            )
        };
        fs::write(&profile, env("/usr/bin:/bin"))?;
        assert!(Nix::is_usable(&profile));

        fs::write(&profile, env("/nix/store/0000-missing/bin:/bin"))?;
        assert!(!Nix::is_usable(&profile));
        Ok(())
    }

    #[test]
    fn lock_profile_creates_lock_file() -> Result<()> {
        let root = tempdir()?;
        let lock = Nix::lock_profile(root.path())?;
        assert!(root.path().join(Nix::PROFILE_LOCK).exists());
        drop(lock);
        Nix::lock_profile(root.path())?;
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
    fn remove_leftovers_success() -> Result<()> {
        let root = tempdir()?;
        let git = root.path().join(Nix::DIR).join(".git");
        fs::create_dir_all(git.join("objects"))?;
        let dockerignore = root.path().join(".dockerignore");
        fs::write(&dockerignore, Nix::DOCKERIGNORE)?;
        Nix::remove_leftovers(root.path())?;
        assert!(!git.exists());
        assert!(!dockerignore.exists());

        // Custom files and missing leftovers are fine
        fs::write(&dockerignore, "custom\n")?;
        Nix::remove_leftovers(root.path())?;
        assert!(dockerignore.exists());
        Ok(())
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
