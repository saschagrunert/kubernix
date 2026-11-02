//! Public key infrastructure (PKI) for cluster TLS.
//!
//! Generates a self-signed CA and per-component certificates using
//! `cfssl`/`cfssljson`. Certificates cover the admin user, API server,
//! etcd, kubelet nodes, controller-manager, scheduler, proxy, and service
//! accounts. A separate front proxy CA signs the client certificate the API
//! server uses to authenticate against aggregated API servers.

use crate::{Config, kubeconfig::KubeConfig, network::Network, node::Node};
use anyhow::{Context, Result, bail};
use log::{debug, info, warn};
use rayon::prelude::*;
use serde_json::{Value, json, to_string_pretty};
use std::{
    fs::{self, create_dir_all},
    net::Ipv4Addr,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

const ECDSA_KEY_SIZE: u32 = 256;
const CERT_EXPIRY: &str = "87600h";
/// The CAs outlive the certificates they sign, cfssl defaults to 5 years.
const CA_EXPIRY: &str = "175200h";

#[must_use]
pub struct Pki {
    admin: Identity,
    apiserver: Identity,
    ca: Identity,
    controller_manager: Identity,
    etcd: Identity,
    front_proxy_ca: Identity,
    front_proxy_client: Identity,
    kubelets: Vec<Identity>,
    proxy: Identity,
    scheduler: Identity,
    service_account: Identity,
}

impl Pki {
    /// Cluster administrator identity (system:masters group).
    pub fn admin(&self) -> &Identity {
        &self.admin
    }

    /// API server TLS and etcd client identity.
    pub fn apiserver(&self) -> &Identity {
        &self.apiserver
    }

    /// Certificate authority used to sign all other certificates.
    pub fn ca(&self) -> &Identity {
        &self.ca
    }

    /// Controller manager identity (system:kube-controller-manager).
    pub fn controller_manager(&self) -> &Identity {
        &self.controller_manager
    }

    /// etcd server and peer TLS identity.
    pub fn etcd(&self) -> &Identity {
        &self.etcd
    }

    /// Certificate authority for the request header authentication of
    /// aggregated API servers.
    pub fn front_proxy_ca(&self) -> &Identity {
        &self.front_proxy_ca
    }

    /// Client identity of the API server when proxying requests to
    /// aggregated API servers, signed by the front proxy CA.
    pub fn front_proxy_client(&self) -> &Identity {
        &self.front_proxy_client
    }

    /// Per-node kubelet identities (`system:node:<name>`).
    pub fn kubelets(&self) -> &[Identity] {
        &self.kubelets
    }

    /// Kube-proxy identity (system:kube-proxy).
    pub fn proxy(&self) -> &Identity {
        &self.proxy
    }

    /// Scheduler identity (system:kube-scheduler).
    pub fn scheduler(&self) -> &Identity {
        &self.scheduler
    }

    /// Key pair used to sign and verify ServiceAccount tokens.
    pub fn service_account(&self) -> &Identity {
        &self.service_account
    }

    /// All identities, which have to be extended for new ones.
    fn identities(&self) -> Vec<&Identity> {
        let mut identities = vec![
            &self.admin,
            &self.apiserver,
            &self.ca,
            &self.controller_manager,
            &self.etcd,
            &self.front_proxy_ca,
            &self.front_proxy_client,
            &self.proxy,
            &self.scheduler,
            &self.service_account,
        ];
        identities.extend(&self.kubelets);
        identities
    }

    /// Returns true if the files of all identities exist.
    fn complete(&self) -> bool {
        self.identities()
            .iter()
            .all(|i| i.cert().exists() && i.key().exists())
    }
}

#[must_use]
pub struct Identity {
    name: String,
    user: String,
    cert: PathBuf,
    key: PathBuf,
}

impl Identity {
    /// Short identifier used for file naming (e.g. `admin`, `kube-proxy`).
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Kubernetes user or CN embedded in the certificate.
    pub fn user(&self) -> &str {
        &self.user
    }

    /// Path to the PEM-encoded certificate file.
    pub fn cert(&self) -> &Path {
        &self.cert
    }

    /// Path to the PEM-encoded private key file.
    pub fn key(&self) -> &Path {
        &self.key
    }

    /// Create a new identity with derived cert/key paths under `dir`.
    pub fn new(dir: &Path, name: &str, user: &str) -> Identity {
        Identity {
            cert: dir.join(format!("{}.pem", name)),
            key: dir.join(format!("{}-key.pem", name)),
            name: name.into(),
            user: user.into(),
        }
    }
}

struct PkiConfig<'a> {
    ca: &'a Identity,
    front_proxy_ca: &'a Identity,
    ca_config: PathBuf,
    dir: &'a Path,
    apiserver_hosts: Vec<String>,
    node_ips: Vec<String>,
}

impl<'a> PkiConfig<'a> {
    fn ca(&self) -> &Identity {
        self.ca
    }

    fn front_proxy_ca(&self) -> &Identity {
        self.front_proxy_ca
    }

    fn ca_config(&self) -> &PathBuf {
        &self.ca_config
    }

    fn dir(&self) -> &Path {
        self.dir
    }

    fn apiserver_hosts(&self) -> &[String] {
        &self.apiserver_hosts
    }

    fn node_ips(&self) -> &[String] {
        &self.node_ips
    }
}

const ADMIN_NAME: &str = "admin";
const APISERVER_NAME: &str = "kubernetes";
const CA_NAME: &str = "ca";
const CONTROLLER_MANAGER_NAME: &str = "kube-controller-manager";
const CONTROLLER_MANAGER_USER: &str = "system:kube-controller-manager";
const ETCD_NAME: &str = "etcd";
const FRONT_PROXY_CA_NAME: &str = "front-proxy-ca";
const FRONT_PROXY_CLIENT_NAME: &str = "front-proxy-client";
const PROXY_NAME: &str = "kube-proxy";
const PROXY_USER: &str = "system:kube-proxy";
const SCHEDULER_NAME: &str = "kube-scheduler";
const SCHEDULER_USER: &str = "system:kube-scheduler";
const SERVICE_ACCOUNT_NAME: &str = "service-account";
const SERVICE_ACCOUNT_USER: &str = "service-accounts";

impl Pki {
    /// Generate or load all cluster certificates.
    ///
    /// If the PKI directory already exists, identities are loaded from
    /// the existing files. Otherwise, new CAs and all component
    /// certificates are generated via cfssl.
    pub fn new(config: &Config, network: &Network) -> Result<Pki> {
        let dir = &config.root().join("pki");
        // The node name is the hostname in single node mode
        let nodes = (0..config.nodes())
            .map(|n| Node::name(config, network, n))
            .collect::<Vec<String>>();

        // Create the CA only if necessary
        if dir.exists() {
            let pki = Pki {
                admin: Identity::new(dir, ADMIN_NAME, ADMIN_NAME),
                apiserver: Identity::new(dir, APISERVER_NAME, APISERVER_NAME),
                ca: Identity::new(dir, CA_NAME, CA_NAME),
                controller_manager: Identity::new(
                    dir,
                    CONTROLLER_MANAGER_NAME,
                    CONTROLLER_MANAGER_USER,
                ),
                etcd: Identity::new(dir, ETCD_NAME, ETCD_NAME),
                front_proxy_ca: Identity::new(dir, FRONT_PROXY_CA_NAME, FRONT_PROXY_CA_NAME),
                front_proxy_client: Identity::new(
                    dir,
                    FRONT_PROXY_CLIENT_NAME,
                    FRONT_PROXY_CLIENT_NAME,
                ),
                kubelets: nodes
                    .iter()
                    .map(|n| Self::kubelet_identity(dir, n))
                    .collect(),
                proxy: Identity::new(dir, PROXY_NAME, PROXY_USER),
                scheduler: Identity::new(dir, SCHEDULER_NAME, SCHEDULER_USER),
                service_account: Identity::new(dir, SERVICE_ACCOUNT_NAME, SERVICE_ACCOUNT_USER),
            };

            // A failed generation leaves an incomplete directory behind
            if !pki.complete() {
                warn!("PKI directory is incomplete, regenerating it");
                fs::remove_dir_all(dir).context("Unable to remove incomplete PKI directory")?;

                // The kubeconfigs embed the certificates
                let kubeconfig = config.root().join(KubeConfig::DIR);
                if kubeconfig.exists() {
                    fs::remove_dir_all(&kubeconfig)
                        .context("Unable to remove outdated kubeconfig directory")?;
                }
                return Self::new(config, network);
            }
            info!("PKI directory already exists, skipping generation");
            Ok(pki)
        } else {
            info!("Generating certificates");
            create_dir_all(dir).context("Unable to create PKI directory")?;
            let ca_config = Self::write_ca_config(dir)?;
            let (ca, front_proxy_ca) = rayon::join(
                || Self::setup_ca(dir, CA_NAME, APISERVER_NAME),
                || Self::setup_ca(dir, FRONT_PROXY_CA_NAME, FRONT_PROXY_CA_NAME),
            );
            let (ca, front_proxy_ca) = (ca?, front_proxy_ca?);

            let pki_config = &PkiConfig {
                dir,
                ca: &ca,
                front_proxy_ca: &front_proxy_ca,
                ca_config,
                apiserver_hosts: Self::apiserver_hosts(network)?,
                node_ips: Network::host_ips()?
                    .iter()
                    .map(ToString::to_string)
                    .collect(),
            };

            // Generate all leaf certificates in parallel since they
            // only depend on the CA, not on each other.
            type CertFn = fn(&PkiConfig) -> Result<Identity>;
            let cert_fns: [CertFn; 8] = [
                Self::setup_admin,
                Self::setup_apiserver,
                Self::setup_controller_manager,
                Self::setup_etcd,
                Self::setup_front_proxy_client,
                Self::setup_proxy,
                Self::setup_scheduler,
                Self::setup_service_account,
            ];

            let (certs, kubelets) = rayon::join(
                || {
                    cert_fns
                        .into_par_iter()
                        .map(|f| f(pki_config))
                        .collect::<Vec<_>>()
                },
                || {
                    nodes
                        .par_iter()
                        .map(|n| Self::setup_kubelet(pki_config, n))
                        .collect::<Result<Vec<_>>>()
                },
            );

            let mut it = certs.into_iter();
            let admin = it.next().context("missing admin cert result")??;
            let apiserver = it.next().context("missing apiserver cert result")??;
            let controller_manager = it
                .next()
                .context("missing controller-manager cert result")??;
            let etcd = it.next().context("missing etcd cert result")??;
            let front_proxy_client = it
                .next()
                .context("missing front-proxy-client cert result")??;
            let proxy = it.next().context("missing proxy cert result")??;
            let scheduler = it.next().context("missing scheduler cert result")??;
            let service_account = it.next().context("missing service-account cert result")??;
            debug_assert!(it.next().is_none());

            Ok(Pki {
                admin,
                apiserver,
                controller_manager,
                etcd,
                front_proxy_client,
                proxy,
                scheduler,
                service_account,
                kubelets: kubelets?,
                ca,
                front_proxy_ca,
            })
        }
    }

    /// The subject alternative names of the API server certificate.
    fn apiserver_hosts(network: &Network) -> Result<Vec<String>> {
        Ok(vec![
            network.api()?.to_string(),
            Ipv4Addr::LOCALHOST.to_string(),
            network.hostname().into(),
            "kubernetes".into(),
            "kubernetes.default".into(),
            "kubernetes.default.svc".into(),
            "kubernetes.default.svc.cluster.local".into(),
        ])
    }

    /// The kubelet identity of the node. The files are prefixed, because
    /// the node name is the hostname in single node mode, which may
    /// collide with the name of another component.
    fn kubelet_identity(dir: &Path, node: &str) -> Identity {
        Identity::new(dir, &format!("kubelet-{}", node), &Self::node_user(node))
    }

    fn setup_ca(dir: &Path, name: &str, cn: &str) -> Result<Identity> {
        debug!("Creating CA certificates for {}", name);
        let csr = dir.join(format!("{}-csr.json", name));
        let mut content = Self::csr(cn, cn);
        content["ca"] = json!({ "expiry": CA_EXPIRY });
        Self::write_csr(&content, &csr)?;

        let cfssl = Command::new("cfssl")
            .arg("gencert")
            .arg("-initca")
            .arg(csr)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .with_context(|| format!("Unable to spawn cfssl for CA '{}'", name))?;

        Self::pipe_cfssl_to_cfssljson(cfssl, &dir.join(name), name)?;
        debug!("CA certificates created for {}", name);
        Ok(Identity::new(dir, name, name))
    }

    /// The kubelet certificate is used as client and serving certificate.
    /// The API server connects to the kubelet via the node name, or via the
    /// node IP the kubelet reports as InternalIP. All nodes share the
    /// network namespace (of the host, or of rootlesskit in rootless mode),
    /// so its addresses are the candidates for the InternalIP.
    fn setup_kubelet(pki_config: &PkiConfig, node: &str) -> Result<Identity> {
        let identity = Self::kubelet_identity(pki_config.dir(), node);
        let csr_file = pki_config
            .dir()
            .join(format!("{}-csr.json", identity.name()));
        Self::write_csr(&Self::csr(identity.user(), "system:nodes"), &csr_file)?;
        let mut hosts = vec![node.into(), Ipv4Addr::LOCALHOST.to_string()];
        hosts.extend_from_slice(pki_config.node_ips());
        Self::generate(
            pki_config,
            pki_config.ca(),
            identity.name(),
            &csr_file,
            identity.user(),
            &hosts,
        )
    }

    fn setup_admin(pki_config: &PkiConfig) -> Result<Identity> {
        Self::setup_client(pki_config, ADMIN_NAME, ADMIN_NAME, "system:masters")
    }

    fn setup_controller_manager(pki_config: &PkiConfig) -> Result<Identity> {
        Self::setup_client(
            pki_config,
            CONTROLLER_MANAGER_NAME,
            CONTROLLER_MANAGER_USER,
            CONTROLLER_MANAGER_USER,
        )
    }

    fn setup_etcd(pki_config: &PkiConfig) -> Result<Identity> {
        let csr_file = pki_config.dir().join("etcd-csr.json");
        Self::write_csr(&Self::csr(ETCD_NAME, ETCD_NAME), &csr_file)?;
        Self::generate(
            pki_config,
            pki_config.ca(),
            ETCD_NAME,
            &csr_file,
            ETCD_NAME,
            &[Ipv4Addr::LOCALHOST.to_string(), "localhost".into()],
        )
    }

    fn setup_front_proxy_client(pki_config: &PkiConfig) -> Result<Identity> {
        let csr_file = pki_config.dir().join("front-proxy-client-csr.json");
        Self::write_csr(
            &Self::csr(FRONT_PROXY_CLIENT_NAME, FRONT_PROXY_CLIENT_NAME),
            &csr_file,
        )?;
        Self::generate(
            pki_config,
            pki_config.front_proxy_ca(),
            FRONT_PROXY_CLIENT_NAME,
            &csr_file,
            FRONT_PROXY_CLIENT_NAME,
            &[],
        )
    }

    fn setup_proxy(pki_config: &PkiConfig) -> Result<Identity> {
        Self::setup_client(pki_config, PROXY_NAME, PROXY_USER, "system:node-proxier")
    }

    fn setup_scheduler(pki_config: &PkiConfig) -> Result<Identity> {
        Self::setup_client(pki_config, SCHEDULER_NAME, SCHEDULER_USER, SCHEDULER_USER)
    }

    fn setup_apiserver(pki_config: &PkiConfig) -> Result<Identity> {
        let csr_file = pki_config.dir().join("kubernetes-csr.json");
        Self::write_csr(&Self::csr(APISERVER_NAME, APISERVER_NAME), &csr_file)?;
        Self::generate(
            pki_config,
            pki_config.ca(),
            APISERVER_NAME,
            &csr_file,
            APISERVER_NAME,
            pki_config.apiserver_hosts(),
        )
    }

    fn setup_service_account(pki_config: &PkiConfig) -> Result<Identity> {
        Self::setup_client(
            pki_config,
            SERVICE_ACCOUNT_NAME,
            SERVICE_ACCOUNT_USER,
            "kubernetes",
        )
    }

    /// Generate a client certificate signed by the cluster CA, which needs
    /// no subject alternative names.
    fn setup_client(pki_config: &PkiConfig, name: &str, cn: &str, o: &str) -> Result<Identity> {
        let csr_file = pki_config.dir().join(format!("{}-csr.json", name));
        Self::write_csr(&Self::csr(cn, o), &csr_file)?;
        Self::generate(pki_config, pki_config.ca(), name, &csr_file, cn, &[])
    }

    fn generate(
        pki_config: &PkiConfig,
        ca: &Identity,
        name: &str,
        csr: &Path,
        user: &str,
        hosts: &[String],
    ) -> Result<Identity> {
        debug!("Creating certificate for {}", name);

        let mut cmd = Command::new("cfssl");
        cmd.arg("gencert")
            .arg(format!("-ca={}", ca.cert().display()))
            .arg(format!("-ca-key={}", ca.key().display()))
            .arg(format!("-config={}", pki_config.ca_config().display()))
            .arg("-profile=kubernetes");
        if !hosts.is_empty() {
            cmd.arg(format!("-hostname={}", hosts.join(",")));
        }
        let cfssl = cmd
            .arg(csr)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .with_context(|| format!("Unable to spawn cfssl for '{}'", name))?;

        Self::pipe_cfssl_to_cfssljson(cfssl, &pki_config.dir().join(name), name)?;
        debug!("Certificate created for {}", name);

        Ok(Identity::new(pki_config.dir(), name, user))
    }

    /// Pipe cfssl output to cfssljson for certificate generation.
    fn pipe_cfssl_to_cfssljson(
        mut cfssl: std::process::Child,
        output_prefix: &Path,
        name: &str,
    ) -> Result<()> {
        let pipe = cfssl.stdout.take().context("Unable to get cfssl stdout")?;
        let output = Command::new("cfssljson")
            .arg("-bare")
            .arg(output_prefix)
            .stdin(pipe)
            .output()
            .with_context(|| format!("Unable to run cfssljson for '{}'", name))?;

        let cfssl_output = cfssl
            .wait_with_output()
            .with_context(|| format!("Unable to wait for cfssl output for '{}'", name))?;
        if !output.status.success() {
            debug!(
                "cfssl stderr: {}",
                String::from_utf8_lossy(&cfssl_output.stderr)
            );
            debug!("cfssljson output: {:?}", output);
            bail!("Unable to generate certificate for {}", name);
        }

        let key_file = output_prefix.with_file_name(format!("{}-key.pem", name));
        if key_file.exists() {
            fs::set_permissions(&key_file, fs::Permissions::from_mode(0o600)).with_context(
                || format!("Unable to restrict permissions on '{}'", key_file.display()),
            )?;
        }
        Ok(())
    }

    fn csr(cn: &str, o: &str) -> Value {
        json!({
            "CN": cn,
            "key": {
                "algo": "ecdsa",
                "size": ECDSA_KEY_SIZE
            },
            "names": [{
                "O": o,
                "OU": "kubernetes",
            }]
        })
    }

    fn write_csr(csr: &Value, dest: &Path) -> Result<()> {
        fs::write(dest, to_string_pretty(csr)?)
            .with_context(|| format!("Unable to write CSR to '{}'", dest.display()))?;
        Ok(())
    }

    fn write_ca_config(dir: &Path) -> Result<PathBuf> {
        let cfg = json!({
            "signing": {
                "default": {
                    "expiry": CERT_EXPIRY
                },
                "profiles": {
                    "kubernetes": {
                        "usages": [
                            "signing",
                            "server auth",
                            "client auth"
                        ],
                        "expiry": CERT_EXPIRY
                    }
                }
            }
        });
        let dest = dir.join("ca-config.json");
        fs::write(&dest, to_string_pretty(&cfg)?).context("Unable to write CA config")?;
        Ok(dest)
    }

    /// Retrieve the node user
    fn node_user(node: &str) -> String {
        format!("system:node:{}", node)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        config::tests::{test_config, test_config_wrong_root},
        network::tests::test_network,
    };

    #[test]
    fn new_success() -> Result<()> {
        let c = test_config()?;
        let n = test_network()?;
        let _pki = Pki::new(&c, &n)?;
        Ok(())
    }

    fn certinfo(identity: &Identity) -> Result<Value> {
        let output = Command::new("cfssl")
            .args(["certinfo", "-cert"])
            .arg(identity.cert())
            .output()?;
        assert!(output.status.success(), "{:?}", output);
        Ok(serde_json::from_slice(&output.stdout)?)
    }

    fn sans(identity: &Identity) -> Result<Vec<String>> {
        Ok(certinfo(identity)?["sans"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|s| s.as_str().map(Into::into))
                    .collect()
            })
            .unwrap_or_default())
    }

    #[test]
    fn certificates_have_own_sans() -> Result<()> {
        let c = test_config()?;
        let n = test_network()?;
        let pki = Pki::new(&c, &n)?;

        let apiserver = sans(pki.apiserver())?;
        for host in [
            &n.api()?.to_string(),
            "127.0.0.1",
            n.hostname(),
            "kubernetes",
            "kubernetes.default",
            "kubernetes.default.svc",
            "kubernetes.default.svc.cluster.local",
        ] {
            assert!(apiserver.iter().any(|s| s == host), "{} missing", host);
        }
        assert!(
            !apiserver
                .iter()
                .any(|s| s == "kubernetes.svc.cluster.local")
        );

        let mut etcd = sans(pki.etcd())?;
        etcd.sort();
        assert_eq!(etcd, ["127.0.0.1", "localhost"]);

        let kubelet = &pki.kubelets()[0];
        let mut kubelet_sans = sans(kubelet)?;
        kubelet_sans.sort();
        let mut expected = vec!["127.0.0.1".to_string(), n.hostname().into()];
        expected.extend(Network::host_ips()?.iter().map(ToString::to_string));
        expected.sort();
        expected.dedup();
        assert_eq!(kubelet_sans, expected);

        for client in [
            pki.admin(),
            pki.controller_manager(),
            pki.front_proxy_client(),
            pki.proxy(),
            pki.scheduler(),
        ] {
            assert!(sans(client)?.is_empty(), "{} has SANs", client.name());
        }
        Ok(())
    }

    #[test]
    fn kubelet_certificate_does_not_collide() -> Result<()> {
        let c = test_config()?;
        let n = test_network()?;
        let pki = Pki::new(&c, &n)?;
        let kubelet = &pki.kubelets()[0];
        assert_eq!(kubelet.name(), format!("kubelet-{}", n.hostname()));
        assert_eq!(
            certinfo(kubelet)?["subject"]["common_name"],
            format!("system:node:{}", n.hostname())
        );
        Ok(())
    }

    #[test]
    fn front_proxy_client_signed_by_front_proxy_ca() -> Result<()> {
        let c = test_config()?;
        let n = test_network()?;
        let pki = Pki::new(&c, &n)?;
        let client = certinfo(pki.front_proxy_client())?;
        assert_eq!(client["subject"]["common_name"], FRONT_PROXY_CLIENT_NAME);
        assert_eq!(client["issuer"]["common_name"], FRONT_PROXY_CA_NAME);
        assert_eq!(
            certinfo(pki.apiserver())?["issuer"]["common_name"],
            APISERVER_NAME
        );
        Ok(())
    }

    #[test]
    fn ca_outlives_certificates() -> Result<()> {
        let c = test_config()?;
        let n = test_network()?;
        let pki = Pki::new(&c, &n)?;
        let not_after = |i: &Identity| -> Result<String> {
            Ok(certinfo(i)?["not_after"]
                .as_str()
                .context("missing not_after")?
                .into())
        };
        assert!(not_after(pki.ca())? > not_after(pki.apiserver())?);
        assert!(not_after(pki.front_proxy_ca())? > not_after(pki.front_proxy_client())?);
        Ok(())
    }

    #[test]
    fn new_regenerates_incomplete() -> Result<()> {
        let c = test_config()?;
        let n = test_network()?;
        create_dir_all(c.root().join("pki"))?;
        create_dir_all(c.root().join(KubeConfig::DIR))?;
        let pki = Pki::new(&c, &n)?;
        assert!(pki.complete());
        assert!(!c.root().join(KubeConfig::DIR).exists());
        Ok(())
    }

    #[test]
    fn new_failure() -> Result<()> {
        let c = test_config_wrong_root()?;
        let n = test_network()?;
        assert!(Pki::new(&c, &n).is_err());
        Ok(())
    }
}
