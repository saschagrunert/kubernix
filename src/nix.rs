use crate::{Config, system::System};
use anyhow::{Context, Result, bail};
use log::{debug, info};
use serde_json::Value;
use std::{
    env::{current_exe, var},
    fs::{self, create_dir_all},
    path::Path,
    process::Command,
};

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
                dir.join("nixpkgs.json"),
                include_str!("../nix/nixpkgs.json"),
            )?;
            fs::write(dir.join("nixpkgs.nix"), include_str!("../nix/nixpkgs.nix"))?;
            fs::write(dir.join("default.nix"), include_str!("../nix/default.nix"))?;

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
        }

        // Run the shell, forwarding all config options
        let exe = format!("{}", current_exe()?.display());
        let root = format!("{}", config.root().display());
        let log_level = format!("{}", config.log_level());
        let cidr = format!("{}", config.cidr());
        let nodes = format!("{}", config.nodes());
        let container_runtime = config.container_runtime();

        let shell_val = config.shell().clone().unwrap_or_default();
        let overlay_val = config
            .overlay()
            .as_ref()
            .map(|o| format!("{}", o.display()));
        let package_vals: Vec<String> = config.packages().clone();
        let mut args = vec![
            exe.as_str(),
            "--root",
            root.as_str(),
            "--log-level",
            log_level.as_str(),
            "--cidr",
            cidr.as_str(),
            "--nodes",
            nodes.as_str(),
            "--container-runtime",
            container_runtime.as_str(),
        ];

        if let Some(ref overlay) = overlay_val {
            args.push("--overlay");
            args.push(overlay.as_str());
        }

        for pkg in &package_vals {
            args.push("--packages");
            args.push(pkg.as_str());
        }

        let addon_vals: Vec<String> = config.addons().clone();
        for addon in &addon_vals {
            args.push("--addons");
            args.push(addon.as_str());
        }

        if *config.no_shell() {
            args.push("--no-shell");
        } else if !shell_val.is_empty() {
            args.push("--shell");
            args.push(&shell_val);
        }

        Self::run(&config, &args)
    }

    /// Run a command inside the nix shell
    pub fn run(config: &Config, args: &[&str]) -> Result<()> {
        let nix_dir = config.root().join(Self::DIR);

        // Point NIX_PATH at our pinned nixpkgs so nix-shell can resolve
        // <nixpkgs> for bashInteractive without requiring a global NIX_PATH.
        let nixpkgs_url = Self::nixpkgs_url(&nix_dir)?;

        let status = Command::new(System::find_executable("nix-shell")?)
            .env(Self::NIX_ENV, "true")
            .arg("-I")
            .arg(format!("nixpkgs={}", nixpkgs_url))
            .arg(nix_dir.join("default.nix"))
            .arg("--run")
            .arg(args.join(" "))
            .status()?;
        if !status.success() {
            bail!("nix-shell exited with status {}", status);
        }
        Ok(())
    }

    /// Read nixpkgs.json and return the tarball URL for the pinned revision
    fn nixpkgs_url(nix_dir: &Path) -> Result<String> {
        let json_str = fs::read_to_string(nix_dir.join("nixpkgs.json"))
            .context("Unable to read nixpkgs.json")?;
        let json: Value =
            serde_json::from_str(&json_str).context("Unable to parse nixpkgs.json")?;
        let url = json["url"]
            .as_str()
            .context("missing url in nixpkgs.json")?;
        let rev = json["rev"]
            .as_str()
            .context("missing rev in nixpkgs.json")?;
        Ok(format!("{}/archive/{}.tar.gz", url, rev))
    }

    /// Returns true if running in nix environment
    pub fn is_active() -> bool {
        var(Nix::NIX_ENV).is_ok()
    }
}
