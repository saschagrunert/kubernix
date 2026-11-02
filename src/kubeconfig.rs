//! Kubeconfig file generation for cluster components.
//!
//! Creates kubeconfig files for each identity (admin, kubelet, proxy,
//! controller-manager, scheduler) as JSON, which is valid YAML. The files
//! embed the client keys, so only their owner can read them. When running
//! via sudo, the admin kubeconfig belongs to the invoking user.

use crate::{
    API_SERVER_PORT, Config,
    pki::{Identity, Pki},
    write_private,
};
use anyhow::{Context, Result};
use base64::{Engine, engine::general_purpose::STANDARD};
use log::{debug, info, warn};
use nix::unistd::{Gid, Uid, fchown};
use rayon::prelude::*;
use serde::Serialize;
use std::{
    env,
    fs::{self, File, create_dir_all},
    net::Ipv4Addr,
    path::{Path, PathBuf},
};

/// The name of the cluster in the kubeconfigs.
const CLUSTER: &str = "kubernetes";
/// The name of the context in the kubeconfigs.
const CONTEXT: &str = "kubernix";

/// A kubeconfig with a single cluster, user and context, as written by
/// `kubectl config` with embedded certificates.
#[derive(Debug, PartialEq, Serialize)]
#[cfg_attr(test, derive(serde::Deserialize))]
#[serde(rename_all = "kebab-case")]
struct Document {
    #[serde(rename = "apiVersion")]
    api_version: String,
    kind: String,
    clusters: Vec<NamedCluster>,
    users: Vec<NamedUser>,
    contexts: Vec<NamedContext>,
    current_context: String,
}

#[derive(Debug, PartialEq, Serialize)]
#[cfg_attr(test, derive(serde::Deserialize))]
struct NamedCluster {
    name: String,
    cluster: Cluster,
}

#[derive(Debug, PartialEq, Serialize)]
#[cfg_attr(test, derive(serde::Deserialize))]
#[serde(rename_all = "kebab-case")]
struct Cluster {
    certificate_authority_data: String,
    server: String,
}

#[derive(Debug, PartialEq, Serialize)]
#[cfg_attr(test, derive(serde::Deserialize))]
struct NamedUser {
    name: String,
    user: User,
}

#[derive(Debug, PartialEq, Serialize)]
#[cfg_attr(test, derive(serde::Deserialize))]
#[serde(rename_all = "kebab-case")]
struct User {
    client_certificate_data: String,
    client_key_data: String,
}

#[derive(Debug, PartialEq, Serialize)]
#[cfg_attr(test, derive(serde::Deserialize))]
struct NamedContext {
    name: String,
    context: KubeContext,
}

#[derive(Debug, PartialEq, Serialize)]
#[cfg_attr(test, derive(serde::Deserialize))]
struct KubeContext {
    cluster: String,
    user: String,
}

impl Document {
    /// Build the kubeconfig of the identity, which embeds the base64
    /// encoded certificates.
    fn new(identity: &Identity, ca: &Path) -> Result<Document> {
        let read = |path: &Path| {
            fs::read(path)
                .map(|data| STANDARD.encode(data))
                .with_context(|| format!("Unable to read '{}'", path.display()))
        };
        Ok(Document {
            api_version: "v1".into(),
            kind: "Config".into(),
            clusters: vec![NamedCluster {
                name: CLUSTER.into(),
                cluster: Cluster {
                    certificate_authority_data: read(ca)?,
                    server: format!("https://{}:{API_SERVER_PORT}", Ipv4Addr::LOCALHOST),
                },
            }],
            users: vec![NamedUser {
                name: identity.user().into(),
                user: User {
                    client_certificate_data: read(identity.cert())?,
                    client_key_data: read(identity.key())?,
                },
            }],
            contexts: vec![NamedContext {
                name: CONTEXT.into(),
                context: KubeContext {
                    cluster: CLUSTER.into(),
                    user: identity.user().into(),
                },
            }],
            current_context: CONTEXT.into(),
        })
    }
}

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
        let document = Document::new(identity, ca)?;
        write_private(&kubeconfig, &serde_json::to_vec_pretty(&document)?)?;
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
    use serde::Deserialize;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn new_success() -> Result<()> {
        let c = test_config()?;
        let n = test_network()?;
        let p = Pki::new(&c, &n)?;
        let kc = KubeConfig::new(&c, &p)?;
        for file in [
            kc.admin(),
            kc.controller_manager(),
            kc.proxy(),
            kc.scheduler(),
            &kc.kubelets()[0],
        ] {
            let mode = fs::metadata(file)?.permissions().mode();
            assert_eq!(mode & 0o777, 0o600, "{}", file.display());
        }
        assert_eq!(
            kc.kubelets()[0].file_name().and_then(|f| f.to_str()),
            Some(format!("kubelet-{}.kubeconfig", n.hostname()).as_str())
        );
        Ok(())
    }

    fn decode(data: &str) -> Result<Vec<u8>> {
        Ok(STANDARD.decode(data)?)
    }

    #[test]
    fn kubeconfigs_embed_identities() -> Result<()> {
        let c = test_config()?;
        let n = test_network()?;
        let p = Pki::new(&c, &n)?;
        let kc = KubeConfig::new(&c, &p)?;
        let ca = fs::read(p.ca().cert())?;

        for (file, identity) in [
            (kc.admin(), p.admin()),
            (kc.controller_manager(), p.controller_manager()),
            (kc.proxy(), p.proxy()),
            (kc.scheduler(), p.scheduler()),
            (&kc.kubelets()[0], &p.kubelets()[0]),
        ] {
            let document: Document = serde_json::from_slice(&fs::read(file)?)?;
            assert_eq!(document, Document::new(identity, p.ca().cert())?);
            assert_eq!(document.api_version, "v1");
            assert_eq!(document.kind, "Config");
            assert_eq!(document.current_context, "kubernix");

            let [cluster] = &document.clusters[..] else {
                panic!("expected a single cluster");
            };
            assert_eq!(cluster.name, "kubernetes");
            assert_eq!(cluster.cluster.server, "https://127.0.0.1:6443");
            assert_eq!(decode(&cluster.cluster.certificate_authority_data)?, ca);

            let [user] = &document.users[..] else {
                panic!("expected a single user");
            };
            assert_eq!(user.name, identity.user());
            assert_eq!(
                decode(&user.user.client_certificate_data)?,
                fs::read(identity.cert())?
            );
            assert_eq!(
                decode(&user.user.client_key_data)?,
                fs::read(identity.key())?
            );

            let [context] = &document.contexts[..] else {
                panic!("expected a single context");
            };
            assert_eq!(context.name, "kubernix");
            assert_eq!(context.context.cluster, "kubernetes");
            assert_eq!(context.context.user, identity.user());
        }
        Ok(())
    }

    #[test]
    fn kubeconfig_format() -> Result<()> {
        let document = Document {
            api_version: "v1".into(),
            kind: "Config".into(),
            clusters: vec![NamedCluster {
                name: CLUSTER.into(),
                cluster: Cluster {
                    certificate_authority_data: "Y2E=".into(),
                    server: "https://127.0.0.1:6443".into(),
                },
            }],
            users: vec![NamedUser {
                name: "admin".into(),
                user: User {
                    client_certificate_data: "Y2VydA==".into(),
                    client_key_data: "a2V5".into(),
                },
            }],
            contexts: vec![NamedContext {
                name: CONTEXT.into(),
                context: KubeContext {
                    cluster: CLUSTER.into(),
                    user: "admin".into(),
                },
            }],
            current_context: CONTEXT.into(),
        };

        // The keys match the ones of kubectl
        let value = serde_json::to_value(&document)?;
        assert_eq!(
            value,
            serde_json::json!({
                "apiVersion": "v1",
                "kind": "Config",
                "clusters": [{
                    "name": "kubernetes",
                    "cluster": {
                        "certificate-authority-data": "Y2E=",
                        "server": "https://127.0.0.1:6443",
                    },
                }],
                "users": [{
                    "name": "admin",
                    "user": {
                        "client-certificate-data": "Y2VydA==",
                        "client-key-data": "a2V5",
                    },
                }],
                "contexts": [{
                    "name": "kubernix",
                    "context": {
                        "cluster": "kubernetes",
                        "user": "admin",
                    },
                }],
                "current-context": "kubernix",
            })
        );
        assert_eq!(Document::deserialize(value)?, document);
        Ok(())
    }

    #[test]
    fn new_failure_missing_certificates() -> Result<()> {
        let c = test_config()?;
        let dir = c.root().join("missing");
        let identity = Identity::new(&dir, "admin", "admin");
        assert!(KubeConfig::setup_kubeconfig(c.root(), &identity, &dir.join("ca.pem")).is_err());
        assert!(!c.root().join("admin.kubeconfig").exists());
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
