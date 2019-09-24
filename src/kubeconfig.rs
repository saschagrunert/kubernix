use crate::{pki::Pki, Config};
use failure::{bail, Fallible};
use log::debug;
use std::{
    fs::create_dir_all,
    path::{Path, PathBuf},
    process::Command,
};

#[derive(Default)]
pub struct KubeConfig {
    proxy: PathBuf,
    controller_manager: PathBuf,
    scheduler: PathBuf,
    admin: PathBuf,
}

impl KubeConfig {
    pub fn new(config: &Config, pki: &Pki) -> Fallible<KubeConfig> {
        // Create the target dir
        let kube_dir = &config.root.join(&config.kube.dir);
        create_dir_all(kube_dir)?;

        let mut kube = KubeConfig::default();
        kube.setup_proxy(kube_dir, &pki)?;
        kube.setup_controller_manager(kube_dir, &pki)?;
        kube.setup_scheduler(kube_dir, &pki)?;
        kube.setup_admin(kube_dir, &pki)?;

        Ok(kube)
    }

    fn setup_proxy(&mut self, dir: &Path, pki: &Pki) -> Fallible<()> {
        const NAME: &str = "kube-proxy";
        let target = self.setup_kubeconfig(
            dir,
            NAME,
            &format!("system:{}", NAME),
            &pki.ca,
            &pki.proxy_cert,
            &pki.proxy_key,
        )?;
        self.proxy = target;
        Ok(())
    }

    fn setup_controller_manager(
        &mut self,
        dir: &Path,
        pki: &Pki,
    ) -> Fallible<()> {
        const NAME: &str = "kube-controller-manager";
        let target = self.setup_kubeconfig(
            dir,
            NAME,
            &format!("system:{}", NAME),
            &pki.ca,
            &pki.controller_manager_cert,
            &pki.controller_manager_key,
        )?;
        self.controller_manager = target;
        Ok(())
    }

    fn setup_scheduler(&mut self, dir: &Path, pki: &Pki) -> Fallible<()> {
        const NAME: &str = "kube-scheduler";
        let target = self.setup_kubeconfig(
            dir,
            NAME,
            &format!("system:{}", NAME),
            &pki.ca,
            &pki.scheduler_cert,
            &pki.scheduler_key,
        )?;
        self.scheduler = target;
        Ok(())
    }

    fn setup_admin(&mut self, dir: &Path, pki: &Pki) -> Fallible<()> {
        const NAME: &str = "admin";
        let target = self.setup_kubeconfig(
            dir,
            NAME,
            NAME,
            &pki.ca,
            &pki.admin_cert,
            &pki.admin_key,
        )?;
        self.admin = target;
        Ok(())
    }

    fn setup_kubeconfig(
        &mut self,
        dir: &Path,
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
            .arg("--server=https://127.0.0.1:6443")
            .arg(&kubeconfig_arg)
            .output()?;
        if !output.status.success() {
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
            bail!("Kubectl set-context command failed");
        }

        let output = Command::new("kubectl")
            .arg("config")
            .arg("use-context")
            .arg("default")
            .arg(&kubeconfig_arg)
            .output()?;
        if !output.status.success() {
            bail!("Kubectl use-context command failed");
        }

        debug!("Kubeconfig created for {}", name);
        Ok(target)
    }
}
