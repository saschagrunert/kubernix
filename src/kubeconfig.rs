use crate::{
    Config,
    kubectl::Kubectl,
    pki::{Identity, Pki},
};
use anyhow::{Context, Result, format_err};
use log::{debug, info};
use nix::sys::stat::{Mode, fchmod};
use std::{
    fs::{File, create_dir_all},
    net::Ipv4Addr,
    path::{Path, PathBuf},
};

pub struct KubeConfig {
    kubelets: Vec<PathBuf>,
    proxy: PathBuf,
    controller_manager: PathBuf,
    scheduler: PathBuf,
    admin: PathBuf,
}

impl KubeConfig {
    pub fn kubelets(&self) -> &Vec<PathBuf> {
        &self.kubelets
    }

    pub fn proxy(&self) -> &PathBuf {
        &self.proxy
    }

    pub fn controller_manager(&self) -> &PathBuf {
        &self.controller_manager
    }

    pub fn scheduler(&self) -> &PathBuf {
        &self.scheduler
    }

    pub fn admin(&self) -> &PathBuf {
        &self.admin
    }

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

    fn setup_kubeconfig(dir: &Path, identity: &Identity, ca: &Path) -> Result<PathBuf> {
        debug!("Creating kubeconfig for {}", identity.name());
        let kubeconfig = Self::target_config(dir, identity);

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
            identity.user(),
            &format!("--client-certificate={}", identity.cert().display()),
            &format!("--client-key={}", identity.key().display()),
            embed_certs,
        ])?;

        let context = "kubernix";
        kubectl.config(&[
            "set-context",
            context,
            &format!("--cluster={}", cluster),
            &format!("--user={}", identity.user()),
        ])?;

        kubectl.config(&["use-context", context])?;

        // Make kubeconfig readable for non-root users spawning additional
        // shell sessions via `kubernix shell`
        let file = File::open(&kubeconfig).context("unable to open kubeconfig")?;
        fchmod(
            &file,
            Mode::from_bits(0o644).ok_or_else(|| format_err!("unable to get mode bits"))?,
        )
        .context("unable to set kubeconfig permissions")?;

        debug!("Kubeconfig created for {}", identity.name());
        Ok(kubeconfig)
    }

    fn target_config(dir: &Path, identity: &Identity) -> PathBuf {
        dir.join(format!("{}.kubeconfig", identity.name()))
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
