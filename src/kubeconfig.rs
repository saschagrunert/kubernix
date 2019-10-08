use crate::{pki::Pki, system::System, Config};
use failure::{bail, Fallible};
use getset::Getters;
use log::{debug, info};
use std::{
    fs::create_dir_all,
    net::Ipv4Addr,
    path::{Path, PathBuf},
    process::Command,
};

#[derive(Default, Getters)]
pub struct KubeConfig {
    #[get = "pub"]
    kubelet: PathBuf,

    #[get = "pub"]
    proxy: PathBuf,

    #[get = "pub"]
    controller_manager: PathBuf,

    #[get = "pub"]
    scheduler: PathBuf,

    #[get = "pub"]
    admin: PathBuf,
}

impl KubeConfig {
    pub fn new(config: &Config, system: &System, pki: &Pki) -> Fallible<KubeConfig> {
        info!("Creating kubeconfigs");

        // Create the target dir
        let dir = config.root().join("kubeconfig");
        create_dir_all(&dir)?;

        let mut kube = KubeConfig::default();
        kube.kubelet = Self::setup_kubelet(&dir, &pki, system.ip(), system.hostname())?;
        kube.proxy = Self::setup_proxy(&dir, &pki, system.ip())?;
        kube.controller_manager = Self::setup_controller_manager(&dir, &pki)?;
        kube.scheduler = Self::setup_scheduler(&dir, &pki)?;
        kube.admin = Self::setup_admin(&dir, &pki)?;

        Ok(kube)
    }

    fn setup_kubelet(dir: &Path, pki: &Pki, ip: &str, hostname: &str) -> Fallible<PathBuf> {
        Ok(Self::setup_kubeconfig(
            dir,
            ip,
            hostname,
            &format!("system:node:{}", hostname),
            pki.ca().cert(),
            pki.kubelet().cert(),
            pki.kubelet().key(),
        )?)
    }

    fn setup_proxy(dir: &Path, pki: &Pki, ip: &str) -> Fallible<PathBuf> {
        const NAME: &str = "kube-proxy";
        Ok(Self::setup_kubeconfig(
            dir,
            ip,
            NAME,
            &format!("system:{}", NAME),
            pki.ca().cert(),
            pki.proxy().cert(),
            pki.proxy().key(),
        )?)
    }

    fn setup_controller_manager(dir: &Path, pki: &Pki) -> Fallible<PathBuf> {
        const NAME: &str = "kube-controller-manager";
        Ok(Self::setup_kubeconfig(
            dir,
            &Ipv4Addr::LOCALHOST.to_string(),
            NAME,
            &format!("system:{}", NAME),
            pki.ca().cert(),
            pki.controller_manager().cert(),
            pki.controller_manager().key(),
        )?)
    }

    fn setup_scheduler(dir: &Path, pki: &Pki) -> Fallible<PathBuf> {
        const NAME: &str = "kube-scheduler";
        Ok(Self::setup_kubeconfig(
            dir,
            &Ipv4Addr::LOCALHOST.to_string(),
            NAME,
            &format!("system:{}", NAME),
            pki.ca().cert(),
            pki.scheduler().cert(),
            pki.scheduler().key(),
        )?)
    }

    fn setup_admin(dir: &Path, pki: &Pki) -> Fallible<PathBuf> {
        const NAME: &str = "admin";
        Ok(Self::setup_kubeconfig(
            dir,
            &Ipv4Addr::LOCALHOST.to_string(),
            NAME,
            NAME,
            pki.ca().cert(),
            pki.admin().cert(),
            pki.admin().key(),
        )?)
    }

    fn setup_kubeconfig(
        dir: &Path,
        ip: &str,
        name: &str,
        user: &str,
        ca: &Path,
        cert: &Path,
        key: &Path,
    ) -> Fallible<PathBuf> {
        debug!("Creating kubeconfig for {}", name);
        let target = dir.join(format!("{}.kubeconfig", name));
        let kubeconfig_arg = format!("--kubeconfig={}", target.display());

        let output = Command::new("kubectl")
            .arg("config")
            .arg("set-cluster")
            .arg("kubernetes")
            .arg(format!("--certificate-authority={}", ca.display()))
            .arg("--embed-certs=true")
            .arg(format!("--server=https://{}:6443", ip))
            .arg(&kubeconfig_arg)
            .output()?;
        if !output.status.success() {
            debug!(
                "kubectl set-cluster stdout: {}",
                String::from_utf8(output.stdout)?
            );
            debug!(
                "kubectl set-cluster stderr: {}",
                String::from_utf8(output.stderr)?
            );
            bail!("Kubectl set-cluster command failed");
        }

        let output = Command::new("kubectl")
            .arg("config")
            .arg("set-credentials")
            .arg(user)
            .arg(format!("--client-certificate={}", cert.display()))
            .arg(format!("--client-key={}", key.display()))
            .arg("--embed-certs=true")
            .arg(&kubeconfig_arg)
            .output()?;
        if !output.status.success() {
            debug!(
                "kubectl set-credentials stdout: {}",
                String::from_utf8(output.stdout)?
            );
            debug!(
                "kubectl set-credentials stderr: {}",
                String::from_utf8(output.stderr)?
            );
            bail!("Kubectl set-credentials command failed");
        }

        let output = Command::new("kubectl")
            .arg("config")
            .arg("set-context")
            .arg("default")
            .arg("--cluster=kubernetes")
            .arg(format!("--user={}", user))
            .arg(&kubeconfig_arg)
            .output()?;
        if !output.status.success() {
            debug!(
                "kubectl set-context stdout: {}",
                String::from_utf8(output.stdout)?
            );
            debug!(
                "kubectl set-context stderr: {}",
                String::from_utf8(output.stderr)?
            );
            bail!("Kubectl set-context command failed");
        }

        let output = Command::new("kubectl")
            .arg("config")
            .arg("use-context")
            .arg("default")
            .arg(&kubeconfig_arg)
            .output()?;
        if !output.status.success() {
            debug!(
                "kubectl use-context stdout: {}",
                String::from_utf8(output.stdout)?
            );
            debug!(
                "kubectl use-context stderr: {}",
                String::from_utf8(output.stderr)?
            );
            bail!("Kubectl use-context command failed");
        }

        debug!("Kubeconfig created for {}", name);
        Ok(target)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{config::tests::test_config, network::tests::test_network};

    #[test]
    fn new_success() -> Fallible<()> {
        let c = test_config()?;
        let n = test_network()?;
        let s = System::default();
        let p = Pki::new(&c, &s, &n)?;
        KubeConfig::new(&c, &s, &p)?;
        Ok(())
    }
}
