//! Public key infrastructure (PKI) for cluster TLS.
//!
//! Generates a self-signed CA and per-component certificates in process.
//! Certificates cover the admin user, API server, etcd, kubelet nodes,
//! controller-manager, scheduler, proxy, and service accounts. A separate
//! front proxy CA signs the client certificate the API server uses to
//! authenticate against aggregated API servers. All keys are ECDSA P-256
//! keys, which every component accepts, including for signing
//! ServiceAccount tokens.

use crate::{Config, kubeconfig::KubeConfig, network::Network, node::Node, write_private};
use anyhow::{Context, Result};
use log::{debug, info, warn};
use rayon::prelude::*;
use rcgen::{
    BasicConstraints, CertificateParams, DistinguishedName, DnType, ExtendedKeyUsagePurpose, IsCa,
    Issuer, KeyPair, KeyUsagePurpose, PKCS_ECDSA_P256_SHA256,
};
use std::{
    fs::{self, create_dir_all},
    net::Ipv4Addr,
    path::{Path, PathBuf},
};
use time::{Duration, OffsetDateTime};

/// The validity of the leaf certificates, 10 years.
const CERT_VALIDITY: Duration = Duration::hours(87_600);
/// The CAs outlive the certificates they sign, 20 years.
const CA_VALIDITY: Duration = Duration::hours(175_200);
/// Backdate the certificates to tolerate clock skew.
const CERT_BACKDATE: Duration = Duration::minutes(5);
/// The organizational unit of all certificates.
const ORGANIZATIONAL_UNIT: &str = "kubernetes";

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

/// A certificate authority, which keeps its key in memory to sign the
/// leaf certificates.
struct Ca {
    identity: Identity,
    issuer: Issuer<'static, KeyPair>,
}

struct PkiConfig<'a> {
    ca: &'a Ca,
    front_proxy_ca: &'a Ca,
    dir: &'a Path,
    apiserver_hosts: Vec<String>,
    node_ips: Vec<String>,
}

impl<'a> PkiConfig<'a> {
    fn ca(&self) -> &Ca {
        self.ca
    }

    fn front_proxy_ca(&self) -> &Ca {
        self.front_proxy_ca
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
    /// certificates are generated.
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
            let (ca, front_proxy_ca) = rayon::join(
                || Self::setup_ca(dir, CA_NAME, APISERVER_NAME),
                || Self::setup_ca(dir, FRONT_PROXY_CA_NAME, FRONT_PROXY_CA_NAME),
            );
            let (ca, front_proxy_ca) = (ca?, front_proxy_ca?);

            let pki_config = &PkiConfig {
                dir,
                ca: &ca,
                front_proxy_ca: &front_proxy_ca,
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
                ca: ca.identity,
                front_proxy_ca: front_proxy_ca.identity,
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

    fn setup_ca(dir: &Path, name: &str, cn: &str) -> Result<Ca> {
        debug!("Creating CA certificates for {}", name);
        let key = Self::key_pair(name)?;
        let mut params = Self::params(cn, cn, &[], CA_VALIDITY)?;
        params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
        let cert = params
            .self_signed(&key)
            .with_context(|| format!("Unable to self sign CA '{}'", name))?;

        let identity = Identity::new(dir, name, name);
        Self::write(&identity, &cert.pem(), &key)?;
        debug!("CA certificates created for {}", name);
        Ok(Ca {
            identity,
            issuer: Issuer::new(params, key),
        })
    }

    /// The kubelet certificate is used as client and serving certificate.
    /// The API server connects to the kubelet via the node name, or via the
    /// node IP the kubelet reports as InternalIP. All nodes share the
    /// network namespace (of the host, or of rootlesskit in rootless mode),
    /// so its addresses are the candidates for the InternalIP.
    fn setup_kubelet(pki_config: &PkiConfig, node: &str) -> Result<Identity> {
        let identity = Self::kubelet_identity(pki_config.dir(), node);
        let mut hosts = vec![node.into(), Ipv4Addr::LOCALHOST.to_string()];
        hosts.extend_from_slice(pki_config.node_ips());
        Self::generate(pki_config.ca(), identity, "system:nodes", &hosts)
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
        Self::generate(
            pki_config.ca(),
            Identity::new(pki_config.dir(), ETCD_NAME, ETCD_NAME),
            ETCD_NAME,
            &[Ipv4Addr::LOCALHOST.to_string(), "localhost".into()],
        )
    }

    fn setup_front_proxy_client(pki_config: &PkiConfig) -> Result<Identity> {
        Self::generate(
            pki_config.front_proxy_ca(),
            Identity::new(
                pki_config.dir(),
                FRONT_PROXY_CLIENT_NAME,
                FRONT_PROXY_CLIENT_NAME,
            ),
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
        Self::generate(
            pki_config.ca(),
            Identity::new(pki_config.dir(), APISERVER_NAME, APISERVER_NAME),
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
        Self::generate(
            pki_config.ca(),
            Identity::new(pki_config.dir(), name, cn),
            o,
            &[],
        )
    }

    /// Generate the certificate of the identity, whose common name is the
    /// user, signed by the CA. The certificates are used for server and
    /// client authentication.
    fn generate(ca: &Ca, identity: Identity, o: &str, hosts: &[String]) -> Result<Identity> {
        let name = identity.name();
        debug!("Creating certificate for {}", name);
        let key = Self::key_pair(name)?;
        let mut params = Self::params(identity.user(), o, hosts, CERT_VALIDITY)?;
        params.is_ca = IsCa::ExplicitNoCa;
        params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
        params.extended_key_usages = vec![
            ExtendedKeyUsagePurpose::ServerAuth,
            ExtendedKeyUsagePurpose::ClientAuth,
        ];
        params.use_authority_key_identifier_extension = true;
        let cert = params
            .signed_by(&key, &ca.issuer)
            .with_context(|| format!("Unable to sign certificate for '{}'", name))?;

        Self::write(&identity, &cert.pem(), &key)?;
        debug!("Certificate created for {}", name);
        Ok(identity)
    }

    fn key_pair(name: &str) -> Result<KeyPair> {
        KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256)
            .with_context(|| format!("Unable to generate key for '{}'", name))
    }

    /// The certificate parameters with the subject and validity. Hosts
    /// which are IP addresses become IP subject alternative names, all
    /// others DNS names.
    fn params(
        cn: &str,
        o: &str,
        hosts: &[String],
        validity: Duration,
    ) -> Result<CertificateParams> {
        let mut params = CertificateParams::new(hosts)
            .with_context(|| format!("Invalid subject alternative names for '{}'", cn))?;
        let mut dn = DistinguishedName::new();
        dn.push(DnType::OrganizationName, o);
        dn.push(DnType::OrganizationalUnitName, ORGANIZATIONAL_UNIT);
        dn.push(DnType::CommonName, cn);
        params.distinguished_name = dn;
        params.not_before = OffsetDateTime::now_utc() - CERT_BACKDATE;
        params.not_after = params.not_before + validity;
        Ok(params)
    }

    /// Write the PEM encoded certificate and key of the identity. Only the
    /// owner can read the key.
    fn write(identity: &Identity, cert: &str, key: &KeyPair) -> Result<()> {
        fs::write(identity.cert(), cert).with_context(|| {
            format!(
                "Unable to write certificate '{}'",
                identity.cert().display()
            )
        })?;
        write_private(identity.key(), key.serialize_pem().as_bytes())
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
    use anyhow::bail;
    use rcgen::PublicKeyData;
    use std::{net::IpAddr, os::unix::fs::PermissionsExt};
    use x509_parser::{
        certificate::X509Certificate,
        extensions::{GeneralName, ParsedExtension},
        pem::{Pem, parse_x509_pem},
    };

    #[test]
    fn new_success() -> Result<()> {
        let c = test_config()?;
        let n = test_network()?;
        let _pki = Pki::new(&c, &n)?;
        Ok(())
    }

    fn pem(path: &Path) -> Result<Pem> {
        let data = fs::read(path)?;
        let (rest, pem) = parse_x509_pem(&data)?;
        assert!(rest.is_empty(), "{}", path.display());
        Ok(pem)
    }

    /// Run the check against the parsed certificate of the identity.
    fn with_cert<T>(identity: &Identity, check: impl FnOnce(&X509Certificate) -> T) -> Result<T> {
        let pem = pem(identity.cert())?;
        assert_eq!(pem.label, "CERTIFICATE");
        Ok(check(&pem.parse_x509()?))
    }

    /// The authority and subject key identifiers of the certificate.
    fn key_ids<'a>(cert: &'a X509Certificate<'a>) -> (Option<&'a [u8]>, Option<&'a [u8]>) {
        let (mut aki, mut ski) = (None, None);
        for ext in cert.extensions() {
            match ext.parsed_extension() {
                ParsedExtension::AuthorityKeyIdentifier(a) => {
                    aki = a.key_identifier.as_ref().map(|k| k.0)
                }
                ParsedExtension::SubjectKeyIdentifier(k) => ski = Some(k.0),
                _ => {}
            }
        }
        (aki, ski)
    }

    fn sans(identity: &Identity) -> Result<Vec<String>> {
        with_cert(identity, |cert| -> Result<Vec<String>> {
            let mut sans = Vec::new();
            if let Some(ext) = cert.subject_alternative_name()? {
                for name in &ext.value.general_names {
                    sans.push(match name {
                        GeneralName::DNSName(dns) => dns.to_string(),
                        GeneralName::IPAddress(ip) => {
                            IpAddr::from(<[u8; 4]>::try_from(*ip)?).to_string()
                        }
                        other => bail!("unexpected SAN {:?}", other),
                    });
                }
            }
            sans.sort();
            Ok(sans)
        })?
    }

    fn leaves(pki: &Pki) -> Vec<&Identity> {
        let mut leaves = vec![
            pki.admin(),
            pki.apiserver(),
            pki.controller_manager(),
            pki.etcd(),
            pki.front_proxy_client(),
            pki.proxy(),
            pki.scheduler(),
            pki.service_account(),
        ];
        leaves.extend(pki.kubelets());
        leaves
    }

    #[test]
    fn certificates_have_own_sans() -> Result<()> {
        let c = test_config()?;
        let n = test_network()?;
        let pki = Pki::new(&c, &n)?;

        let mut apiserver = vec![
            n.api()?.to_string(),
            "127.0.0.1".into(),
            n.hostname().into(),
            "kubernetes".into(),
            "kubernetes.default".into(),
            "kubernetes.default.svc".into(),
            "kubernetes.default.svc.cluster.local".into(),
        ];
        apiserver.sort();
        assert_eq!(sans(pki.apiserver())?, apiserver);

        assert_eq!(sans(pki.etcd())?, ["127.0.0.1", "localhost"]);

        let kubelet = &pki.kubelets()[0];
        let mut expected = vec!["127.0.0.1".to_string(), n.hostname().into()];
        expected.extend(Network::host_ips()?.iter().map(ToString::to_string));
        expected.sort();
        assert_eq!(sans(kubelet)?, expected);

        for client in [
            pki.admin(),
            pki.controller_manager(),
            pki.front_proxy_client(),
            pki.proxy(),
            pki.scheduler(),
            pki.service_account(),
        ] {
            assert!(sans(client)?.is_empty(), "{} has SANs", client.name());
        }
        Ok(())
    }

    #[test]
    fn certificates_have_subjects() -> Result<()> {
        let c = test_config()?;
        let n = test_network()?;
        let pki = Pki::new(&c, &n)?;
        let node = format!("system:node:{}", n.hostname());
        for (identity, common_name, organization) in [
            (pki.ca(), "kubernetes", "kubernetes"),
            (pki.front_proxy_ca(), "front-proxy-ca", "front-proxy-ca"),
            (pki.admin(), "admin", "system:masters"),
            (pki.apiserver(), "kubernetes", "kubernetes"),
            (
                pki.controller_manager(),
                "system:kube-controller-manager",
                "system:kube-controller-manager",
            ),
            (pki.etcd(), "etcd", "etcd"),
            (
                pki.front_proxy_client(),
                "front-proxy-client",
                "front-proxy-client",
            ),
            (&pki.kubelets()[0], &node, "system:nodes"),
            (pki.proxy(), "system:kube-proxy", "system:node-proxier"),
            (
                pki.scheduler(),
                "system:kube-scheduler",
                "system:kube-scheduler",
            ),
            (pki.service_account(), "service-accounts", "kubernetes"),
        ] {
            assert_eq!(
                with_cert(identity, |cert| cert.subject().to_string())?,
                format!("O={}, OU=kubernetes, CN={}", organization, common_name),
            );
        }
        Ok(())
    }

    #[test]
    fn certificates_have_usages() -> Result<()> {
        let c = test_config()?;
        let n = test_network()?;
        let pki = Pki::new(&c, &n)?;

        for ca in [pki.ca(), pki.front_proxy_ca()] {
            with_cert(ca, |cert| -> Result<()> {
                let bc = cert.basic_constraints()?.context("no basic constraints")?;
                assert!(bc.critical && bc.value.ca && bc.value.path_len_constraint.is_none());
                let ku = cert.key_usage()?.context("no key usage")?;
                assert!(ku.critical);
                assert!(ku.value.key_cert_sign() && ku.value.crl_sign());
                assert!(!ku.value.digital_signature());
                assert!(cert.extended_key_usage()?.is_none());
                Ok(())
            })??;
        }

        for leaf in leaves(&pki) {
            with_cert(leaf, |cert| -> Result<()> {
                let bc = cert.basic_constraints()?.context("no basic constraints")?;
                assert!(bc.critical && !bc.value.ca, "{}", leaf.name());
                let ku = cert.key_usage()?.context("no key usage")?;
                assert!(ku.critical);
                assert!(ku.value.digital_signature());
                assert_eq!(ku.value.flags, 1, "{} usages", leaf.name());
                let eku = cert
                    .extended_key_usage()?
                    .context("no extended key usage")?;
                assert!(!eku.critical);
                let eku = eku.value;
                assert!(eku.server_auth && eku.client_auth, "{}", leaf.name());
                assert!(eku.other.is_empty() && !eku.any && !eku.code_signing);
                Ok(())
            })??;
        }
        Ok(())
    }

    #[test]
    fn certificates_signed_by_ca() -> Result<()> {
        let c = test_config()?;
        let n = test_network()?;
        let pki = Pki::new(&c, &n)?;
        let ca_pem = pem(pki.ca().cert())?;
        let ca = ca_pem.parse_x509()?;
        let front_proxy_ca_pem = pem(pki.front_proxy_ca().cert())?;
        let front_proxy_ca = front_proxy_ca_pem.parse_x509()?;

        for cert in [&ca, &front_proxy_ca] {
            assert_eq!(cert.issuer(), cert.subject());
            cert.verify_signature(None)?;
            let (aki, ski) = key_ids(cert);
            assert!(aki.is_none() && ski.is_some());
        }

        for leaf in leaves(&pki) {
            let issuer = if leaf.name() == FRONT_PROXY_CLIENT_NAME {
                &front_proxy_ca
            } else {
                &ca
            };
            with_cert(leaf, |cert| -> Result<()> {
                assert_eq!(cert.issuer(), issuer.subject(), "{}", leaf.name());
                cert.verify_signature(Some(issuer.public_key()))?;
                let (aki, ski) = key_ids(cert);
                assert!(aki.is_some() && ski.is_some(), "{}", leaf.name());
                assert_eq!(aki, key_ids(issuer).1, "{}", leaf.name());
                Ok(())
            })??;
        }

        // The cluster CA must not trust the front proxy client
        with_cert(pki.front_proxy_client(), |cert| {
            assert!(cert.verify_signature(Some(ca.public_key())).is_err());
        })?;
        Ok(())
    }

    #[test]
    fn certificates_validity() -> Result<()> {
        let c = test_config()?;
        let n = test_network()?;
        let pki = Pki::new(&c, &n)?;
        let now = OffsetDateTime::now_utc().unix_timestamp();

        assert_eq!(CA_VALIDITY.whole_days(), 7300);
        assert_eq!(CERT_VALIDITY.whole_days(), 3650);
        let mut identities = vec![(pki.ca(), CA_VALIDITY), (pki.front_proxy_ca(), CA_VALIDITY)];
        identities.extend(leaves(&pki).into_iter().map(|i| (i, CERT_VALIDITY)));
        for (identity, expected) in identities {
            let (not_before, not_after) = with_cert(identity, |cert| {
                let v = cert.validity();
                (v.not_before.timestamp(), v.not_after.timestamp())
            })?;
            assert!(not_before < now, "{}", identity.name());
            assert!(now - not_before < 3600, "{}", identity.name());
            assert_eq!(not_after - not_before, expected.whole_seconds());
        }
        Ok(())
    }

    #[test]
    fn keys_are_private() -> Result<()> {
        let c = test_config()?;
        let n = test_network()?;
        let pki = Pki::new(&c, &n)?;
        let mut identities = leaves(&pki);
        identities.extend([pki.ca(), pki.front_proxy_ca()]);
        for identity in identities {
            let mode = fs::metadata(identity.key())?.permissions().mode();
            assert_eq!(mode & 0o777, 0o600, "{}", identity.key().display());
            let key = KeyPair::from_pem(&fs::read_to_string(identity.key())?)?;
            assert_eq!(key.algorithm(), &PKCS_ECDSA_P256_SHA256);
            // The key belongs to the certificate
            let public_key = with_cert(identity, |cert| cert.public_key().raw.to_vec())?;
            assert_eq!(key.subject_public_key_info(), public_key);
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
            with_cert(kubelet, |cert| cert.subject().to_string())?,
            format!(
                "O=system:nodes, OU=kubernetes, CN=system:node:{}",
                n.hostname()
            )
        );
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
