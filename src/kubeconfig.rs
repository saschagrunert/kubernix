use crate::{pki::Pki, Config, LOCALHOST};
use failure::{bail, Fallible};
use log::{debug, info};
use std::{
    fs::create_dir_all,
    path::{Path, PathBuf},
    process::Command,
};

#[derive(Default)]
pub struct KubeConfig {
    pub kubelet: PathBuf,
    pub proxy: PathBuf,
    pub controller_manager: PathBuf,
    pub scheduler: PathBuf,
    pub admin: PathBuf,
}

impl KubeConfig {
    pub fn new(config: &Config, pki: &Pki, ip: &str, hostname: &str) -> Fallible<KubeConfig> {
        info!("Creating kubeconfigs");

        // Create the target dir
        let kube_dir = &config.root.join(&config.kube.dir);
        create_dir_all(kube_dir)?;

        let mut kube = KubeConfig::default();
        kube.setup_kubelet(kube_dir, &pki, ip, hostname)?;
        kube.setup_proxy(kube_dir, &pki, ip)?;
        kube.setup_controller_manager(kube_dir, &pki, LOCALHOST)?;
        kube.setup_scheduler(kube_dir, &pki, LOCALHOST)?;
        kube.setup_admin(kube_dir, &pki, LOCALHOST)?;

        Ok(kube)
    }

    fn setup_kubelet(&mut self, dir: &Path, pki: &Pki, ip: &str, hostname: &str) -> Fallible<()> {
        let target = self.setup_kubeconfig(
            dir,
            ip,
            hostname,
            &format!("system:node:{}", hostname),
            pki.ca.cert(),
            pki.kubelet.cert(),
            pki.kubelet.key(),
        )?;
        self.kubelet = target;
        Ok(())
    }

    fn setup_proxy(&mut self, dir: &Path, pki: &Pki, ip: &str) -> Fallible<()> {
        const NAME: &str = "kube-proxy";
        let target = self.setup_kubeconfig(
            dir,
            ip,
            NAME,
            &format!("system:{}", NAME),
            pki.ca.cert(),
            pki.proxy.cert(),
            pki.proxy.key(),
        )?;
        self.proxy = target;
        Ok(())
    }

    fn setup_controller_manager(&mut self, dir: &Path, pki: &Pki, ip: &str) -> Fallible<()> {
        const NAME: &str = "kube-controller-manager";
        let target = self.setup_kubeconfig(
            dir,
            ip,
            NAME,
            &format!("system:{}", NAME),
            pki.ca.cert(),
            pki.controller_manager.cert(),
            pki.controller_manager.key(),
        )?;
        self.controller_manager = target;
        Ok(())
    }

    fn setup_scheduler(&mut self, dir: &Path, pki: &Pki, ip: &str) -> Fallible<()> {
        const NAME: &str = "kube-scheduler";
        let target = self.setup_kubeconfig(
            dir,
            ip,
            NAME,
            &format!("system:{}", NAME),
            pki.ca.cert(),
            pki.scheduler.cert(),
            pki.scheduler.key(),
        )?;
        self.scheduler = target;
        Ok(())
    }

    fn setup_admin(&mut self, dir: &Path, pki: &Pki, ip: &str) -> Fallible<()> {
        const NAME: &str = "admin";
        let target = self.setup_kubeconfig(
            dir,
            ip,
            NAME,
            NAME,
            pki.ca.cert(),
            pki.admin.cert(),
            pki.admin.key(),
        )?;
        self.admin = target;
        Ok(())
    }

    fn setup_kubeconfig(
        &mut self,
        dir: &Path,
        ip: &str,
        name: &str,
        user: &str,
        ca: &Path,
        cert: &Path,
        key: &Path,
    ) -> Fallible<PathBuf> {
        debug!("Creating kubeconfig for {}", name);
        let target = Path::new(dir).join(format!("{}.kubeconfig", name));
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
