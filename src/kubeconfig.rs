//! Kubeconfig file generation for cluster components.
//!
//! Creates kubeconfig files for each identity (admin, kubelet, proxy,
//! controller-manager, scheduler) using `kubectl config` commands and
//! sets file permissions so non-root shell sessions can read them.

use crate::{
    Config,
    kubectl::Kubectl,
    pki::{Identity, Pki},
};
use anyhow::{Context, Result};
use log::{debug, info};
use nix::sys::stat::{Mode, fchmod};
use rayon::prelude::*;
use std::{
    fs::{File, create_dir_all},
    net::Ipv4Addr,
    path::{Path, PathBuf},
};

#[must_use]
pub struct KubeConfig {
    kubelets: Vec<PathBuf>,
    proxy: PathBuf,
    controller_manager: PathBuf,
    scheduler: PathBuf,
    admin: PathBuf,
}

impl KubeConfig {
    /// Kubeconfig file paths for each kubelet node.
    pub fn kubelets(&self) -> &[PathBuf] {
        &self.kubelets
    }

    /// Kubeconfig file path for kube-proxy.
    pub fn proxy(&self) -> &Path {
        &self.proxy
    }

    /// Kubeconfig file path for the controller manager.
    pub fn controller_manager(&self) -> &Path {
        &self.controller_manager
    }

    /// Kubeconfig file path for the scheduler.
    pub fn scheduler(&self) -> &Path {
        &self.scheduler
    }

    /// Kubeconfig file path for the cluster admin (used by kubectl).
    pub fn admin(&self) -> &Path {
        &self.admin
    }

    /// Generate or load kubeconfig files for all cluster components.
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

            let ca = pki.ca().cert();

            // Generate all kubeconfig files in parallel since they are
            // independent of each other.
            let (left, right) = rayon::join(
                || {
                    rayon::join(
                        || Self::setup_kubeconfig(&dir, pki.proxy(), ca),
                        || Self::setup_kubeconfig(&dir, pki.controller_manager(), ca),
                    )
                },
                || {
                    rayon::join(
                        || {
                            rayon::join(
                                || Self::setup_kubeconfig(&dir, pki.scheduler(), ca),
                                || Self::setup_kubeconfig(&dir, pki.admin(), ca),
                            )
                        },
                        || {
                            pki.kubelets()
                                .par_iter()
                                .map(|id| Self::setup_kubeconfig(&dir, id, ca))
                                .collect::<Result<Vec<_>, _>>()
                        },
                    )
                },
            );

            let (proxy, controller_manager) = left;
            let ((scheduler, admin), kubelets) = right;

            Ok(KubeConfig {
                kubelets: kubelets?,
                proxy: proxy?,
                controller_manager: controller_manager?,
                scheduler: scheduler?,
                admin: admin?,
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

        // Make kubeconfig readable for non-root users spawning
        // additional shell sessions via `kubernix shell`.
        let file = File::open(&kubeconfig).context("unable to open kubeconfig")?;
        fchmod(&file, Mode::from_bits_truncate(0o644))
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
        let _kc = KubeConfig::new(&c, &p)?;
        Ok(())
    }
}
