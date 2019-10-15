use crate::{
    pki::{Idendity, Pki},
    Config,
};
use failure::{bail, Fallible};
use getset::Getters;
use log::{debug, info};
use std::{
    fs::create_dir_all,
    net::Ipv4Addr,
    path::{Path, PathBuf},
    process::Command,
};

#[derive(Getters)]
pub struct KubeConfig {
    #[get = "pub"]
    kubelets: Vec<PathBuf>,

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
    pub fn new(config: &Config, pki: &Pki) -> Fallible<KubeConfig> {
        // Create the target dir
        let dir = config.root().join("kubeconfig");

        if dir.exists() {
            info!("Kubeconfig directory already exists, skipping generation");

            let kubelets = pki
                .kubelets()
                .iter()
                .map(|i| Self::target_config(&dir, i))
                .collect();

            Ok(KubeConfig {
                kubelets,
                proxy: Self::target_config(&dir, pki.proxy()),
                controller_manager: Self::target_config(&dir, pki.controller_manager()),
                scheduler: Self::target_config(&dir, pki.scheduler()),
                admin: Self::target_config(&dir, pki.admin()),
            })
        } else {
            info!("Creating kubeconfigs");
            create_dir_all(&dir)?;

            let kubelets = pki
                .kubelets()
                .iter()
                .map(|x| Self::setup_kubeconfig(&dir, x, pki.ca().cert()))
                .collect::<Result<Vec<_>, _>>()?;

            Ok(KubeConfig {
                kubelets,
                proxy: Self::setup_kubeconfig(&dir, pki.proxy(), pki.ca().cert())?,
                controller_manager: Self::setup_kubeconfig(
                    &dir,
                    pki.controller_manager(),
                    pki.ca().cert(),
                )?,
                scheduler: Self::setup_kubeconfig(&dir, pki.scheduler(), pki.ca().cert())?,
                admin: Self::setup_kubeconfig(&dir, pki.admin(), pki.ca().cert())?,
            })
        }
    }

    fn setup_kubeconfig(dir: &Path, idendity: &Idendity, ca: &Path) -> Fallible<PathBuf> {
        debug!("Creating kubeconfig for {}", idendity.name());
        let target = Self::target_config(dir, idendity);
        let kubeconfig_arg = format!("--kubeconfig={}", target.display());

        let output = Command::new("kubectl")
            .arg("config")
            .arg("set-cluster")
            .arg("kubernetes")
            .arg(format!("--certificate-authority={}", ca.display()))
            .arg("--embed-certs=true")
            .arg(format!("--server=https://{}:6443", &Ipv4Addr::LOCALHOST))
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
            .arg(idendity.user())
            .arg(format!(
                "--client-certificate={}",
                idendity.cert().display()
            ))
            .arg(format!("--client-key={}", idendity.key().display()))
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
            .arg(format!("--user={}", idendity.user()))
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

        debug!("Kubeconfig created for {}", idendity.name());
        Ok(target)
    }

    fn target_config(dir: &Path, idendity: &Idendity) -> PathBuf {
        dir.join(format!("{}.kubeconfig", idendity.name()))
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
        let p = Pki::new(&c, &n)?;
        KubeConfig::new(&c, &p)?;
        Ok(())
    }
}
