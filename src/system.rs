use failure::{bail, format_err, Fallible};
use log::{debug, info};
use std::{net::IpAddr, process::Command};

pub struct System {
    modules: Vec<String>,
    sysctls: Vec<String>,
}

impl System {
    /// Create a new system
    pub fn new() -> Self {
        Self {
            modules: vec![
                "overlay".to_owned(),
                "br_netfilter".to_owned(),
                "ip_conntrack".to_owned(),
            ],
            sysctls: vec![
                "net.bridge.bridge-nf-call-ip6tables".to_owned(),
                "net.bridge.bridge-nf-call-iptables".to_owned(),
                "net.ipv4.conf.all.route_localnet".to_owned(),
                "net.ipv4.ip_forward".to_owned(),
            ],
        }
    }

    /// Retrieve the local hosts IP via the default route
    pub fn ip(&self) -> Fallible<String> {
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
    pub fn hostname(&self) -> Fallible<String> {
        let hostname =
            hostname::get_hostname().ok_or_else(|| format_err!("Unable to retrieve hostname"))?;
        info!("Using hostname {}", hostname);
        Ok(hostname)
    }

    /// Load all required kernel modules and configure the system
    pub fn prepare(&self) -> Fallible<()> {
        // Load the modules
        for module in &self.modules {
            self.modprobe(module)?;
        }

        // Set the sysctls
        for sysctl in &self.sysctls {
            self.sysctl_enable(sysctl)?;
        }

        Ok(())
    }

    /// Load a single kernel module via 'modprobe'
    fn modprobe(&self, module: &str) -> Fallible<()> {
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
    fn sysctl_enable(&self, key: &str) -> Fallible<()> {
        debug!("Enabling sysctl '{}'", key);
        let enable_arg = format!("{}=1", key);
        let output = Command::new("sysctl").arg("-w").arg(&enable_arg).output()?;
        let stderr = String::from_utf8(output.stderr)?;
        if !stderr.is_empty() {
            bail!("Unable to set sysctl '{}': {}", enable_arg, stderr);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prepare_success_empty() -> Fallible<()> {
        let mut system = System::new();
        system.modules = vec![];
        system.sysctls = vec![];
        system.prepare()
    }

    #[test]
    fn module_failure() {
        let system = System::new();
        assert!(system.modprobe("invalid").is_err());
    }

    #[test]
    fn sysctl_failure() {
        let system = System::new();
        assert!(system.sysctl_enable("invalid").is_err());
    }

    #[test]
    fn ip_success() {
        assert!(System::new().ip().is_ok());
    }

    #[test]
    fn hostname_success() {
        assert!(System::new().hostname().is_ok());
    }
}
