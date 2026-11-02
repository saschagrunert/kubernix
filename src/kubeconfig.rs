//! Kubeconfig file generation for cluster components.
//!
//! Creates kubeconfig files for each identity (admin, kubelet, proxy,
//! controller-manager, scheduler) using `kubectl config` commands. The
//! files embed the client keys, so only their owner can read them. When
//! running via sudo, the admin kubeconfig belongs to the invoking user.

use crate::{
    API_SERVER_PORT, Config,
    kubectl::Kubectl,
    pki::{Identity, Pki},
};
use anyhow::{Context, Result};
use log::{debug, info, warn};
use nix::{
    sys::stat::{Mode, fchmod},
    unistd::{Gid, Uid, fchown},
};
use rayon::prelude::*;
use std::{
    env,
    fs::{self, File, create_dir_all},
    net::Ipv4Addr,
    path::{Path, PathBuf},
};

/// Paths of the generated kubeconfig files for all cluster identities.
#[must_use]
pub struct KubeConfig {
    kubelets: Vec<PathBuf>,
    proxy: PathBuf,
    controller_manager: PathBuf,
    scheduler: PathBuf,
    admin: PathBuf,
}

impl KubeConfig {
    /// The directory of the kubeconfigs in the root.
    pub const DIR: &'static str = "kubeconfig";

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

    /// Returns true if all kubeconfig files exist.
    fn complete(&self) -> bool {
        [
            &self.proxy,
            &self.controller_manager,
            &self.scheduler,
            &self.admin,
        ]
        .into_iter()
        .chain(&self.kubelets)
        .all(|p| p.exists())
    }

    /// Generate or load kubeconfig files for all cluster components.
    pub fn new(config: &Config, pki: &Pki) -> Result<KubeConfig> {
        // Create the target dir
        let dir = config.root().join(Self::DIR);

        if dir.exists() {
            let kubelets = pki
                .kubelets()
                .iter()
                .map(|i| Self::target_config(&dir, i))
                .collect();

            let kubeconfig = KubeConfig {
                kubelets,
                proxy: Self::target_config(&dir, pki.proxy()),
                controller_manager: Self::target_config(&dir, pki.controller_manager()),
                scheduler: Self::target_config(&dir, pki.scheduler()),
                admin: Self::target_config(&dir, pki.admin()),
            };

            // A failed generation leaves an incomplete directory behind
            if !kubeconfig.complete() {
                warn!("Kubeconfig directory is incomplete, regenerating it");
                fs::remove_dir_all(&dir)
                    .context("Unable to remove incomplete kubeconfig directory")?;
                return Self::new(config, pki);
            }
            info!("Kubeconfig directory already exists, skipping generation");
            Ok(kubeconfig)
        } else {
            info!("Creating kubeconfigs");
            create_dir_all(&dir).context("Unable to create kubeconfig directory")?;

            let ca = pki.ca().cert();

            // Generate all kubeconfig files in parallel since they are
            // independent of each other.
            let identities = [
                pki.proxy(),
                pki.controller_manager(),
                pki.scheduler(),
                pki.admin(),
            ];
            let (configs, kubelets) = rayon::join(
                || {
                    identities
                        .into_par_iter()
                        .map(|id| Self::setup_kubeconfig(&dir, id, ca))
                        .collect::<Vec<_>>()
                },
                || {
                    pki.kubelets()
                        .par_iter()
                        .map(|id| Self::setup_kubeconfig(&dir, id, ca))
                        .collect::<Result<Vec<_>>>()
                },
            );

            let mut it = configs.into_iter();
            let proxy = it.next().context("missing proxy kubeconfig result")??;
            let controller_manager = it
                .next()
                .context("missing controller-manager kubeconfig result")??;
            let scheduler = it.next().context("missing scheduler kubeconfig result")??;
            let admin = it.next().context("missing admin kubeconfig result")??;
            debug_assert!(it.next().is_none());

            if let Some((uid, gid)) =
                Self::sudo_owner(Uid::effective().is_root() && !config.is_rootless())
            {
                debug!("Changing the admin kubeconfig owner to {}:{}", uid, gid);
                let file = File::open(&admin).context("Unable to open admin kubeconfig")?;
                fchown(&file, Some(uid), Some(gid))
                    .context("Unable to change the admin kubeconfig owner")?;
            }

            Ok(KubeConfig {
                proxy,
                controller_manager,
                scheduler,
                admin,
                kubelets: kubelets?,
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
            &format!("--server=https://{}:{API_SERVER_PORT}", Ipv4Addr::LOCALHOST),
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

        let file = File::open(&kubeconfig).context("Unable to open kubeconfig")?;
        fchmod(&file, Mode::from_bits_truncate(0o600))
            .context("Unable to set kubeconfig permissions")?;

        debug!("Kubeconfig created for {}", identity.name());
        Ok(kubeconfig)
    }

    fn target_config(dir: &Path, identity: &Identity) -> PathBuf {
        dir.join(format!("{}.kubeconfig", identity.name()))
    }

    /// The user and group which invoked kubernix via sudo, if running as
    /// real root.
    fn sudo_owner(root: bool) -> Option<(Uid, Gid)> {
        if !root {
            return None;
        }
        let uid = env::var("SUDO_UID").ok()?.parse().ok()?;
        let gid = env::var("SUDO_GID").ok()?.parse().ok()?;
        Some((Uid::from_raw(uid), Gid::from_raw(gid)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{config::tests::test_config, network::tests::test_network};
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn new_success() -> Result<()> {
        let c = test_config()?;
        let n = test_network()?;
        let p = Pki::new(&c, &n)?;
        let kc = KubeConfig::new(&c, &p)?;
        for file in [kc.admin(), kc.proxy(), &kc.kubelets()[0]] {
            let mode = fs::metadata(file)?.permissions().mode();
            assert_eq!(mode & 0o777, 0o600, "{}", file.display());
        }
        assert_eq!(
            kc.kubelets()[0].file_name().and_then(|f| f.to_str()),
            Some(format!("kubelet-{}.kubeconfig", n.hostname()).as_str())
        );
        Ok(())
    }

    #[test]
    fn new_regenerates_incomplete() -> Result<()> {
        let c = test_config()?;
        let n = test_network()?;
        let p = Pki::new(&c, &n)?;
        create_dir_all(c.root().join(KubeConfig::DIR))?;
        let kc = KubeConfig::new(&c, &p)?;
        assert!(kc.complete());
        Ok(())
    }

    #[test]
    fn sudo_owner_from_env() {
        temp_env::with_vars(
            [("SUDO_UID", Some("1000")), ("SUDO_GID", Some("100"))],
            || {
                assert_eq!(
                    KubeConfig::sudo_owner(true),
                    Some((Uid::from_raw(1000), Gid::from_raw(100)))
                );
                assert_eq!(KubeConfig::sudo_owner(false), None);
            },
        );
        temp_env::with_vars([("SUDO_UID", None::<&str>), ("SUDO_GID", None)], || {
            assert_eq!(KubeConfig::sudo_owner(true), None);
        });
    }
}
