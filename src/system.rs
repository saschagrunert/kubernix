use crate::{node::Node, Config};
use anyhow::{bail, Context, Result};
use log::{debug, info, warn};
use std::{
    env::{split_paths, var, var_os},
    fmt::Display,
    fs::{self, read_to_string},
    net::Ipv4Addr,
    path::{Path, PathBuf},
    process::Command,
};

pub struct System {
    hosts: Option<String>,
}

impl System {
    /// Create a new system
    pub fn setup(config: &Config) -> Result<Self> {
        if Self::in_container()? {
            info!("Skipping modprobe and sysctl for sake of containerization")
        } else {
            for module in &["overlay", "br_netfilter", "ip_conntrack"] {
                Self::modprobe(module)?;
            }
            for sysctl in &[
                "net.bridge.bridge-nf-call-ip6tables",
                "net.bridge.bridge-nf-call-iptables",
                "net.ipv4.conf.all.route_localnet",
                "net.ipv4.ip_forward",
            ] {
                Self::sysctl_enable(sysctl)?;
            }
        }

        let hosts = if config.multi_node() {
            // Try to write the hostnames, which does not work on every system
            let hosts_file = Self::hosts();
            let hosts = read_to_string(&hosts_file)?;
            let local_hosts = (0..config.nodes())
                .map(|x| format!("{} {}", Ipv4Addr::LOCALHOST, Node::raw(x)))
                .collect::<Vec<_>>();

            let mut new_hosts = hosts
                .lines()
                .filter(|x| !local_hosts.iter().any(|y| x == y))
                .map(|x| x.into())
                .collect::<Vec<_>>();
            new_hosts.extend(local_hosts);

            match fs::write(&hosts_file, new_hosts.join("\n")) {
                Err(e) => {
                    warn!(
                        "Unable to write hosts file '{}'. The nodes may be not reachable: {}",
                        hosts_file.display(),
                        e
                    );
                    None
                }
                _ => Some(hosts),
            }
        } else {
            None
        };

        Ok(Self { hosts })
    }

    /// Returns true if the process is running inside a container
    pub fn in_container() -> Result<bool> {
        Ok(
            read_to_string(PathBuf::from("/").join("proc").join("1").join("cgroup"))
                .context("Unable to retrieve systems container status")?
                .lines()
                .any(|x| x.contains("libpod") || x.contains("podman") || x.contains("docker")),
        )
    }

    /// Restore the initial system state
    pub fn cleanup(&self) {
        if let Some(hosts) = &self.hosts {
            if let Err(e) = fs::write(Self::hosts(), hosts) {
                warn!(
                    "Unable to restore hosts file, may need manual cleanup: {}",
                    e
                )
            }
        }
    }

    /// Find an executable inside the current $PATH environment
    pub fn find_executable<P>(name: P) -> Result<PathBuf>
    where
        P: AsRef<Path> + Display,
    {
        var_os("PATH")
            .and_then(|paths| {
                split_paths(&paths)
                    .filter_map(|dir| {
                        let full_path = dir.join(&name);
                        if full_path.is_file() {
                            Some(full_path)
                        } else {
                            None
                        }
                    })
                    .next()
            })
            .with_context(|| format!("Unable to find executable '{}' in $PATH", name))
    }

    /// Return the full path to the default system shell
    pub fn shell() -> Result<String> {
        let shell = var("SHELL").unwrap_or_else(|_| "sh".into());
        Ok(format!(
            "{}",
            Self::find_executable(&shell)
                .with_context(|| format!("Unable to find system shell '{}'", shell))?
                .display()
        ))
    }

    /// Load a single kernel module via 'modprobe'
    fn modprobe(module: &str) -> Result<()> {
        debug!("Loading kernel module '{}'", module);
        let output = Command::new("modprobe").arg(module).output()?;
        if !output.status.success() {
            bail!(
                "Unable to load '{}' kernel module: {}",
                module,
                String::from_utf8(output.stderr)?,
            );
        }
        Ok(())
    }

    /// Enable a single sysctl by setting it to '1'
    fn sysctl_enable(key: &str) -> Result<()> {
        debug!("Enabling sysctl '{}'", key);
        let enable_arg = format!("{}=1", key);
        let output = Command::new("sysctl").arg("-w").arg(&enable_arg).output()?;
        let stderr = String::from_utf8(output.stderr)?;
        if !stderr.is_empty() {
            bail!("Unable to set sysctl '{}': {}", enable_arg, stderr);
        }
        Ok(())
    }

    fn hosts() -> PathBuf {
        PathBuf::from("/").join("etc").join("hosts")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::env::set_var;

    const VALID_EXECUTABLE: &str = "runc";
    const INVALID_EXECUTABLE: &str = "should-not-exist";

    #[test]
    fn module_failure() {
        assert!(System::modprobe("invalid").is_err());
    }

    #[test]
    fn sysctl_failure() {
        assert!(System::sysctl_enable("invalid").is_err());
    }

    #[test]
    fn find_executable_success() {
        assert!(System::find_executable(VALID_EXECUTABLE).is_ok());
    }

    #[test]
    fn find_executable_failure() {
        assert!(System::find_executable(INVALID_EXECUTABLE).is_err());
    }

    #[test]
    fn find_shell_success() {
        set_var("SHELL", VALID_EXECUTABLE);
        assert!(System::shell().is_ok());
    }
}
