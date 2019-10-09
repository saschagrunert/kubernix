use failure::{bail, format_err, Fallible};
use getset::Getters;
use log::{debug, info};
use std::{
    env::{split_paths, var, var_os},
    fmt::Display,
    net::IpAddr,
    path::{Path, PathBuf},
    process::Command,
};

#[derive(Default, Getters)]
pub struct System {
    #[get = "pub"]
    ip: String,

    #[get = "pub"]
    hostname: String,
}

impl System {
    /// Create a new system
    pub fn new() -> Fallible<Self> {
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
        Ok(Self {
            ip: Self::get_ip()?,
            hostname: Self::get_hostname()?,
        })
    }

    /// Retrieve the local hosts IP via the default route
    fn get_ip() -> Fallible<String> {
        let cmd = Command::new("ip")
            .arg("route")
            .arg("get")
            .arg("1.2.3.4")
            .output()?;
        if !cmd.status.success() {
            bail!("Unable to obtain `ip` output")
        }
        let output = String::from_utf8(cmd.stdout)?;
        let ip = output
            .split_whitespace()
            .nth(6)
            .ok_or_else(|| format_err!("Different `ip` command output expected"))?;
        if let Err(e) = ip.parse::<IpAddr>() {
            bail!("Unable to parse IP '{}': {}", ip, e);
        }
        info!("Using local IP {}", ip);
        Ok(ip.to_owned())
    }

    /// Retrieve the local hostname
    fn get_hostname() -> Fallible<String> {
        let hostname =
            hostname::get_hostname().ok_or_else(|| format_err!("Unable to retrieve hostname"))?;
        info!("Using hostname {}", hostname);
        Ok(hostname)
    }

    /// Load a single kernel module via 'modprobe'
    fn modprobe(module: &str) -> Fallible<()> {
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
    fn sysctl_enable(key: &str) -> Fallible<()> {
        debug!("Enabling sysctl '{}'", key);
        let enable_arg = format!("{}=1", key);
        let output = Command::new("sysctl").arg("-w").arg(&enable_arg).output()?;
        let stderr = String::from_utf8(output.stderr)?;
        if !stderr.is_empty() {
            bail!("Unable to set sysctl '{}': {}", enable_arg, stderr);
        }
        Ok(())
    }

    /// Find an executable inside the current $PATH environment
    pub fn find_executable<P>(name: P) -> Fallible<PathBuf>
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
            .ok_or_else(|| format_err!("Unable to find {} in $PATH", name))
    }

    /// Return the full path to the default system shell
    pub fn shell() -> Fallible<String> {
        Ok(format!(
            "{}",
            Self::find_executable(
                var("SHELL")
                    .map_err(|e| format_err!("Unable to retrieve $SHELL variable: {}", e))?
            )
            .map_err(|e| format_err!("Unable to find system shell {}", e))?
            .display()
        ))
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
    fn ip_success() {
        assert!(System::get_ip().is_ok());
    }

    #[test]
    fn hostname_success() {
        assert!(System::get_hostname().is_ok());
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
