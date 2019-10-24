use crate::{
    kubectl::Kubectl,
    pki::{Idendity, Pki},
    Config,
};
use anyhow::Result;
use getset::Getters;
use log::{debug, info};
use std::{
    fs::create_dir_all,
    net::Ipv4Addr,
    path::{Path, PathBuf},
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
    pub fn new(config: &Config, pki: &Pki) -> Result<KubeConfig> {
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

    fn setup_kubeconfig(dir: &Path, idendity: &Idendity, ca: &Path) -> Result<PathBuf> {
        debug!("Creating kubeconfig for {}", idendity.name());
        let kubeconfig = Self::target_config(dir, idendity);

        let embed_certs = "--embed-certs=true";
        let cluster = "kubernetes";
        let kubectl = Kubectl::new(&kubeconfig);
        kubectl.config(&[
            "set-cluster",
            cluster,
            &format!("--certificate-authority={}", ca.display()),
            &format!("--server=https://{}:6443", &Ipv4Addr::LOCALHOST),
            embed_certs,
        ])?;

        kubectl.config(&[
            "set-credentials",
            &idendity.user(),
            &format!("--client-certificate={}", idendity.cert().display()),
            &format!("--client-key={}", idendity.key().display()),
            embed_certs,
        ])?;

        let context = "default";
        kubectl.config(&[
            "set-context",
            context,
            &format!("--cluster={}", cluster),
            &format!("--user={}", idendity.user()),
        ])?;

        kubectl.config(&["use-context", context])?;

        debug!("Kubeconfig created for {}", idendity.name());
        Ok(kubeconfig)
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
    fn new_success() -> Result<()> {
        let c = test_config()?;
        let n = test_network()?;
        let p = Pki::new(&c, &n)?;
        KubeConfig::new(&c, &p)?;
        Ok(())
    }
}
