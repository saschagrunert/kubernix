//! Public key infrastructure (PKI) for cluster TLS.
//!
//! Generates the certificate authorities (CAs) and the certificates of all
//! components in process, following the layout of kubeadm:
//!
//! - The cluster CA signs the API server serving certificate and the client
//!   certificates of the admin, the components, the kubelets and the API
//!   server (for connections to the kubelets). The controller-manager signs
//!   approved certificate signing requests for client certificates with it.
//! - The etcd CA signs the etcd serving and peer certificates and the etcd
//!   client certificate of the API server, so that no other certificate
//!   grants access to etcd.
//! - The kubelet serving CA signs the serving certificates of the kubelets,
//!   which only the API server trusts for connections to the kubelets, so
//!   that they cannot impersonate the API server. The controller-manager
//!   signs approved kubelet serving certificate signing requests with it.
//! - The front proxy CA signs the client certificate the API server uses to
//!   authenticate against aggregated API servers.
//!
//! Every certificate is either a server or a client certificate, except
//! for the etcd peer certificate, which is both. A plain key pair signs the
//! ServiceAccount tokens. All keys are ECDSA P-256 keys.
//!
//! Existing files get reused if they are valid and match the current
//! cluster, otherwise only the affected files get regenerated. This
//! includes the subject alternative names of the API server and the
//! kubelets. They contain the hostname (for the kubelet only in single node
//! mode, where it is the node name) and, in root mode, the node IPs, which
//! are the bridge gateways (the API server only the advertised one of the
//! first node). In rootless mode, the kubelet certificates contain the
//! fixed addresses of the network namespace of rootlesskit instead. Once
//! etcd contains data, the cluster CA and the ServiceAccount key never get
//! regenerated, since the cluster state depends on them.

use crate::{
    Config, etcd::Etcd, files::create_dir_private, network::Network, node::Node, write_file,
    write_if_changed, write_private,
};
use anyhow::{Context, Result, anyhow, bail};
use log::{debug, info, warn};
use rayon::prelude::*;
use rcgen::{
    BasicConstraints, CertificateParams, DistinguishedName, DnType, ExtendedKeyUsagePurpose, IsCa,
    Issuer, KeyPair, KeyUsagePurpose, PKCS_ECDSA_P256_SHA256, PublicKeyData,
};
use std::{
    fs,
    io::ErrorKind,
    net::{IpAddr, Ipv4Addr, Ipv6Addr},
    path::{Path, PathBuf},
};
use time::{Duration, OffsetDateTime};
use x509_parser::{
    certificate::X509Certificate,
    extensions::GeneralName,
    pem::{Pem, parse_x509_pem},
    prelude::FromDer,
};

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
    apiserver_etcd_client: Identity,
    apiserver_kubelet_client: Identity,
    ca: Identity,
    controller_manager: Identity,
    etcd: Identity,
    etcd_ca: Identity,
    etcd_peer: Identity,
    front_proxy_ca: Identity,
    front_proxy_client: Identity,
    kubelet_ca: Identity,
    kubelet_servers: Vec<Identity>,
    kubelets: Vec<Identity>,
    proxy: Identity,
    scheduler: Identity,
    service_account: ServiceAccountKey,
}

impl Pki {
    /// The directory of the PKI in the root.
    const DIR: &'static str = "pki";

    /// Cluster administrator identity (system:masters group).
    pub fn admin(&self) -> &Identity {
        &self.admin
    }

    /// API server serving identity.
    pub fn apiserver(&self) -> &Identity {
        &self.apiserver
    }

    /// Client identity of the API server for etcd, signed by the etcd CA.
    pub fn apiserver_etcd_client(&self) -> &Identity {
        &self.apiserver_etcd_client
    }

    /// Client identity of the API server for the kubelets.
    pub fn apiserver_kubelet_client(&self) -> &Identity {
        &self.apiserver_kubelet_client
    }

    /// Cluster CA, which signs the API server serving certificate and the
    /// client certificates of the cluster.
    pub fn ca(&self) -> &Identity {
        &self.ca
    }

    /// Controller manager identity (system:kube-controller-manager).
    pub fn controller_manager(&self) -> &Identity {
        &self.controller_manager
    }

    /// etcd serving identity.
    pub fn etcd(&self) -> &Identity {
        &self.etcd
    }

    /// CA of the etcd serving, peer and client certificates.
    pub fn etcd_ca(&self) -> &Identity {
        &self.etcd_ca
    }

    /// etcd peer identity, used as server and client.
    pub fn etcd_peer(&self) -> &Identity {
        &self.etcd_peer
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

    /// CA of the kubelet serving certificates.
    pub fn kubelet_ca(&self) -> &Identity {
        &self.kubelet_ca
    }

    /// Per-node kubelet serving identities, signed by the kubelet CA.
    pub fn kubelet_servers(&self) -> &[Identity] {
        &self.kubelet_servers
    }

    /// Per-node kubelet client identities (`system:node:<name>`).
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
    pub fn service_account(&self) -> &ServiceAccountKey {
        &self.service_account
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

/// The key pair which signs the ServiceAccount tokens.
#[must_use]
pub struct ServiceAccountKey {
    key: PathBuf,
    public_key: PathBuf,
}

impl ServiceAccountKey {
    fn new(dir: &Path) -> Self {
        Self {
            key: dir.join(format!("{}-key.pem", SERVICE_ACCOUNT_NAME)),
            public_key: dir.join(format!("{}.pub", SERVICE_ACCOUNT_NAME)),
        }
    }

    /// Path to the PEM-encoded private key file.
    pub fn key(&self) -> &Path {
        &self.key
    }

    /// Path to the PEM-encoded public key file.
    pub fn public_key(&self) -> &Path {
        &self.public_key
    }
}

/// A certificate authority, which keeps its key in memory to sign the
/// leaf certificates.
struct Ca {
    issuer: Issuer<'static, KeyPair>,
    /// The DER encoded certificate, to verify the leaf certificates
    der: Vec<u8>,
}

/// What a leaf certificate authenticates.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Usage {
    Client,
    Server,
    /// Server and client, like the etcd peers and the etcd server, which
    /// connects to itself
    Peer,
}

impl Usage {
    fn server(self) -> bool {
        matches!(self, Usage::Server | Usage::Peer)
    }

    fn client(self) -> bool {
        matches!(self, Usage::Client | Usage::Peer)
    }

    fn extended_key_usages(self) -> Vec<ExtendedKeyUsagePurpose> {
        let mut usages = vec![];
        if self.server() {
            usages.push(ExtendedKeyUsagePurpose::ServerAuth);
        }
        if self.client() {
            usages.push(ExtendedKeyUsagePurpose::ClientAuth);
        }
        usages
    }
}

/// The specification of a leaf certificate, whose common name is the user
/// of the identity.
struct Leaf<'a> {
    identity: &'a Identity,
    organization: &'a str,
    usage: Usage,
    hosts: Vec<String>,
    ca: &'a Ca,
}

impl Leaf<'_> {
    /// Generate the certificate unless the existing one is valid. Returns
    /// true if it has been generated.
    fn ensure(&self) -> Result<bool> {
        let name = self.identity.name();
        match self.check() {
            Ok(()) => Ok(false),
            Err(e) => {
                debug!("Creating certificate for {} ({:#})", name, e);
                self.generate()?;
                debug!("Certificate created for {}", name);
                Ok(true)
            }
        }
    }

    /// Check that the existing certificate matches the specification.
    fn check(&self) -> Result<()> {
        let pem = Pki::read_cert(self.identity.cert())?;
        let cert = pem.parse_x509()?;
        let key = Pki::read_key(self.identity.key())?;
        Pki::check_cert(&cert, &key, self.identity.user(), self.organization)?;

        let (_, ca) = X509Certificate::from_der(&self.ca.der)?;
        if cert.issuer() != ca.subject() {
            bail!(
                "issued by '{}' instead of '{}'",
                cert.issuer(),
                ca.subject()
            )
        }
        cert.verify_signature(Some(ca.public_key()))
            .context("not signed by the CA")?;

        let eku = cert
            .extended_key_usage()?
            .context("no extended key usage")?
            .value;
        if eku.server_auth != self.usage.server()
            || eku.client_auth != self.usage.client()
            || eku.any
            || !eku.other.is_empty()
        {
            bail!("extended key usages differ from {:?}", self.usage)
        }

        let expected = Pki::normalize_hosts(&self.hosts);
        let actual = Pki::sans(&cert)?;
        if actual != expected {
            bail!(
                "subject alternative names {:?} differ from {:?}",
                actual,
                expected
            )
        }
        Ok(())
    }

    fn generate(&self) -> Result<()> {
        let name = self.identity.name();
        let key = Pki::key_pair(name)?;
        let mut params = Pki::params(
            self.identity.user(),
            self.organization,
            &self.hosts,
            CERT_VALIDITY,
        )?;
        params.is_ca = IsCa::ExplicitNoCa;
        params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
        params.extended_key_usages = self.usage.extended_key_usages();
        params.use_authority_key_identifier_extension = true;
        let cert = params
            .signed_by(&key, &self.ca.issuer)
            .with_context(|| format!("Unable to sign certificate for '{}'", name))?;
        Pki::write(self.identity, &cert.pem(), &key)
    }
}

const ADMIN_NAME: &str = "admin";
const APISERVER_NAME: &str = "kubernetes";
const APISERVER_ETCD_CLIENT_NAME: &str = "apiserver-etcd-client";
const APISERVER_ETCD_CLIENT_USER: &str = "kube-apiserver-etcd-client";
const APISERVER_KUBELET_CLIENT_NAME: &str = "apiserver-kubelet-client";
/// The user of the API server at the kubelets, see `assets/apiserver.yml`.
const APISERVER_KUBELET_CLIENT_USER: &str = "kube-apiserver-kubelet-client";
const CA_NAME: &str = "ca";
const CONTROLLER_MANAGER_NAME: &str = "kube-controller-manager";
const CONTROLLER_MANAGER_USER: &str = "system:kube-controller-manager";
const ETCD_NAME: &str = "etcd";
const ETCD_CA_NAME: &str = "etcd-ca";
const ETCD_PEER_NAME: &str = "etcd-peer";
const FRONT_PROXY_CA_NAME: &str = "front-proxy-ca";
const FRONT_PROXY_CLIENT_NAME: &str = "front-proxy-client";
/// Not prefixed with `kubelet-` to avoid collisions with the files of the
/// kubelets, which contain the node name.
const KUBELET_CA_NAME: &str = "node-serving-ca";
const NODES_ORGANIZATION: &str = "system:nodes";
const PROXY_NAME: &str = "kube-proxy";
const PROXY_USER: &str = "system:kube-proxy";
const SCHEDULER_NAME: &str = "kube-scheduler";
const SCHEDULER_USER: &str = "system:kube-scheduler";
const SERVICE_ACCOUNT_NAME: &str = "service-account";
/// The files of older versions, which are not generated any more: the
/// ServiceAccount certificate, which is a plain key pair now.
const OBSOLETE_FILES: [&str; 1] = ["service-account.pem"];

impl Pki {
    /// Generate or load all cluster certificates.
    ///
    /// Existing files are reused if they are valid and match the current
    /// cluster, all others get generated. Fails if the cluster CA or the
    /// ServiceAccount key would have to be regenerated while etcd contains
    /// data.
    pub fn new(config: &Config, network: &Network) -> Result<Pki> {
        let dir = &config.root().join(Self::DIR);
        create_dir_private(dir).context("Unable to create PKI directory")?;

        // The node name is the hostname in single node mode
        let nodes = (0..config.nodes())
            .map(|n| Node::name(config, network, n))
            .collect::<Vec<String>>();

        let pki = Pki {
            admin: Identity::new(dir, ADMIN_NAME, ADMIN_NAME),
            apiserver: Identity::new(dir, APISERVER_NAME, APISERVER_NAME),
            apiserver_etcd_client: Identity::new(
                dir,
                APISERVER_ETCD_CLIENT_NAME,
                APISERVER_ETCD_CLIENT_USER,
            ),
            apiserver_kubelet_client: Identity::new(
                dir,
                APISERVER_KUBELET_CLIENT_NAME,
                APISERVER_KUBELET_CLIENT_USER,
            ),
            ca: Identity::new(dir, CA_NAME, APISERVER_NAME),
            controller_manager: Identity::new(
                dir,
                CONTROLLER_MANAGER_NAME,
                CONTROLLER_MANAGER_USER,
            ),
            etcd: Identity::new(dir, ETCD_NAME, ETCD_NAME),
            etcd_ca: Identity::new(dir, ETCD_CA_NAME, ETCD_CA_NAME),
            etcd_peer: Identity::new(dir, ETCD_PEER_NAME, ETCD_NAME),
            front_proxy_ca: Identity::new(dir, FRONT_PROXY_CA_NAME, FRONT_PROXY_CA_NAME),
            front_proxy_client: Identity::new(
                dir,
                FRONT_PROXY_CLIENT_NAME,
                FRONT_PROXY_CLIENT_NAME,
            ),
            kubelet_ca: Identity::new(dir, KUBELET_CA_NAME, KUBELET_CA_NAME),
            kubelet_servers: nodes
                .iter()
                .map(|n| Self::kubelet_server_identity(dir, n))
                .collect(),
            kubelets: nodes
                .iter()
                .map(|n| Self::kubelet_identity(dir, n))
                .collect(),
            proxy: Identity::new(dir, PROXY_NAME, PROXY_USER),
            scheduler: Identity::new(dir, SCHEDULER_NAME, SCHEDULER_USER),
            service_account: ServiceAccountKey::new(dir),
        };

        // The cluster state depends on the cluster CA and the
        // ServiceAccount key, all other files can be regenerated.
        let data = Etcd::has_data(config).then(|| Etcd::data_dir(config));
        let data = data.as_deref();
        let ca = Self::setup_ca(pki.ca(), data)?;
        let etcd_ca = Self::setup_ca(pki.etcd_ca(), None)?;
        let front_proxy_ca = Self::setup_ca(pki.front_proxy_ca(), None)?;
        let kubelet_ca = Self::setup_ca(pki.kubelet_ca(), None)?;
        Self::setup_service_account(pki.service_account(), data)?;

        let localhost = Ipv4Addr::LOCALHOST.to_string();
        let etcd_hosts = vec![localhost.clone(), "localhost".into()];
        let node_ips: Vec<String> = network
            .kubelet_ips()?
            .iter()
            .map(ToString::to_string)
            .collect();

        let client = |identity, organization, ca| Leaf {
            identity,
            organization,
            usage: Usage::Client,
            hosts: vec![],
            ca,
        };
        let mut leaves = vec![
            client(pki.admin(), "system:masters", &ca),
            client(pki.apiserver_etcd_client(), APISERVER_NAME, &etcd_ca),
            client(pki.apiserver_kubelet_client(), APISERVER_NAME, &ca),
            client(pki.controller_manager(), CONTROLLER_MANAGER_USER, &ca),
            client(
                pki.front_proxy_client(),
                FRONT_PROXY_CLIENT_NAME,
                &front_proxy_ca,
            ),
            client(pki.proxy(), "system:node-proxier", &ca),
            client(pki.scheduler(), SCHEDULER_USER, &ca),
            Leaf {
                identity: pki.apiserver(),
                organization: APISERVER_NAME,
                usage: Usage::Server,
                hosts: Self::apiserver_hosts(network)?,
                ca: &ca,
            },
            // etcd also connects to its own client port with this
            // certificate (the gRPC gateway), and requires client
            // certificates there, like kubeadm
            Leaf {
                identity: pki.etcd(),
                organization: ETCD_NAME,
                usage: Usage::Peer,
                hosts: etcd_hosts.clone(),
                ca: &etcd_ca,
            },
            Leaf {
                identity: pki.etcd_peer(),
                organization: ETCD_NAME,
                usage: Usage::Peer,
                hosts: etcd_hosts,
                ca: &etcd_ca,
            },
        ];
        for (node, (kubelet, server)) in nodes
            .iter()
            .zip(pki.kubelets().iter().zip(pki.kubelet_servers()))
        {
            leaves.push(client(kubelet, NODES_ORGANIZATION, &ca));
            // The API server connects to the kubelet via the node IP the
            // kubelet reports as InternalIP, or via the node name. All nodes
            // share the network namespace (of the host, or of rootlesskit
            // in rootless mode), so the candidates for the InternalIP are
            // the same for all nodes: the bridge gateways in root mode,
            // which do not change with the network of the host, and the
            // addresses of the network namespace of rootlesskit in rootless
            // mode (see Network::kubelet_ips).
            let mut hosts = vec![node.clone(), localhost.clone()];
            hosts.extend_from_slice(&node_ips);
            leaves.push(Leaf {
                identity: server,
                organization: NODES_ORGANIZATION,
                usage: Usage::Server,
                hosts,
                ca: &kubelet_ca,
            });
        }

        // The leaf certificates only depend on the CAs, not on each other
        let generated = leaves
            .par_iter()
            .map(Leaf::ensure)
            .collect::<Result<Vec<bool>>>()?
            .into_iter()
            .filter(|g| *g)
            .count();
        if generated > 0 {
            info!("Generated {} certificates", generated);
        } else {
            info!("Reusing existing certificates");
        }
        Self::remove_obsolete(dir);
        Ok(pki)
    }

    /// Remove the files of older versions from the PKI directory, which
    /// are not generated any more. Failures are only logged.
    fn remove_obsolete(dir: &Path) {
        for file in OBSOLETE_FILES {
            let path = dir.join(file);
            match fs::remove_file(&path) {
                Ok(()) => debug!("Removed obsolete file '{}'", path.display()),
                Err(e) if e.kind() == ErrorKind::NotFound => {}
                Err(e) => warn!("Unable to remove obsolete file '{}': {}", path.display(), e),
            }
        }
    }

    /// The subject alternative names of the API server certificate,
    /// including the address it advertises in root mode (see
    /// `Network::advertise_address`).
    fn apiserver_hosts(network: &Network) -> Result<Vec<String>> {
        let mut hosts = vec![
            network.api()?.to_string(),
            Ipv4Addr::LOCALHOST.to_string(),
            network.hostname().into(),
            "kubernetes".into(),
            "kubernetes.default".into(),
            "kubernetes.default.svc".into(),
            "kubernetes.default.svc.cluster.local".into(),
        ];
        hosts.extend(network.advertise_address()?.map(|ip| ip.to_string()));
        Ok(hosts)
    }

    /// The kubelet client identity of the node. The files are prefixed,
    /// because the node name is the hostname in single node mode, which may
    /// collide with the name of another component.
    fn kubelet_identity(dir: &Path, node: &str) -> Identity {
        Identity::new(dir, &format!("kubelet-{}", node), &Self::node_user(node))
    }

    /// The kubelet serving identity of the node, whose subject is the same
    /// as the one of the client identity, like the kubelet requests it.
    fn kubelet_server_identity(dir: &Path, node: &str) -> Identity {
        Identity::new(
            dir,
            &format!("kubelet-{}-serving", node),
            &Self::node_user(node),
        )
    }

    /// Load the CA of the identity, or generate it if it is missing or
    /// invalid. A CA which the etcd data in `data` depends on is never
    /// generated again.
    fn setup_ca(identity: &Identity, data: Option<&Path>) -> Result<Ca> {
        let name = identity.name();
        let error = match Self::load_ca(identity) {
            Ok(ca) => return Ok(ca),
            Err(e) => e,
        };
        if let Some(data) = data {
            bail!(
                "The CA '{}' is missing or invalid ({:#}), but the etcd data in '{}' depends \
                 on it. Restore the CA or remove the run directory",
                identity.cert().display(),
                error,
                data.display(),
            )
        }
        debug!("Creating CA certificates for {} ({:#})", name, error);
        let key = Self::key_pair(name)?;
        let mut params = Self::params(identity.user(), identity.user(), &[], CA_VALIDITY)?;
        params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
        let cert = params
            .self_signed(&key)
            .with_context(|| format!("Unable to self sign CA '{}'", name))?;
        Self::write(identity, &cert.pem(), &key)?;
        debug!("CA certificates created for {}", name);
        Ok(Ca {
            der: cert.der().to_vec(),
            issuer: Issuer::new(params, key),
        })
    }

    /// Load the CA of the identity, which has to be a valid self-signed CA
    /// certificate with the matching key.
    fn load_ca(identity: &Identity) -> Result<Ca> {
        let pem = Self::read_cert(identity.cert())?;
        let key = Self::read_key(identity.key())?;
        {
            let cert = pem.parse_x509()?;
            Self::check_cert(&cert, &key, identity.user(), identity.user())?;
            if !cert.is_ca() {
                bail!("not a CA")
            }
            cert.verify_signature(None).context("not self-signed")?;
        }
        let issuer = Issuer::from_ca_cert_der(&pem.contents.as_slice().into(), key)
            .context("Unable to load CA")?;
        Ok(Ca {
            issuer,
            der: pem.contents,
        })
    }

    /// Ensure that the ServiceAccount key pair exists. The key never gets
    /// generated again if the etcd data in `data` exists, since the tokens
    /// stored in the cluster depend on it.
    fn setup_service_account(sa: &ServiceAccountKey, data: Option<&Path>) -> Result<()> {
        let key = match Self::read_key(sa.key()) {
            Ok(key) => key,
            Err(e) => {
                if let Some(data) = data {
                    bail!(
                        "The ServiceAccount key '{}' is missing or invalid ({:#}), but the etcd \
                         data in '{}' depends on it. Restore the key or remove the run directory",
                        sa.key().display(),
                        e,
                        data.display(),
                    )
                }
                debug!("Creating ServiceAccount key ({:#})", e);
                let key = Self::key_pair(SERVICE_ACCOUNT_NAME)?;
                write_private(sa.key(), key.serialize_pem().as_bytes())?;
                key
            }
        };
        write_if_changed(sa.public_key(), &key.public_key_pem())
    }

    /// Read the PEM encoded certificate.
    fn read_cert(path: &Path) -> Result<Pem> {
        let data =
            fs::read(path).with_context(|| format!("Unable to read '{}'", path.display()))?;
        let (_, pem) = parse_x509_pem(&data)
            .map_err(|e| anyhow!("Invalid certificate '{}': {}", path.display(), e))?;
        if pem.label != "CERTIFICATE" {
            bail!("Invalid certificate '{}'", path.display())
        }
        Ok(pem)
    }

    /// Read the PEM encoded ECDSA P-256 key.
    fn read_key(path: &Path) -> Result<KeyPair> {
        let data = fs::read_to_string(path)
            .with_context(|| format!("Unable to read '{}'", path.display()))?;
        let key = KeyPair::from_pem(&data)
            .with_context(|| format!("Invalid key '{}'", path.display()))?;
        if key.algorithm() != &PKCS_ECDSA_P256_SHA256 {
            bail!("Unexpected algorithm of key '{}'", path.display())
        }
        Ok(key)
    }

    /// Check that the certificate is currently valid, belongs to the key
    /// and has the expected subject.
    fn check_cert(cert: &X509Certificate, key: &KeyPair, cn: &str, o: &str) -> Result<()> {
        if !cert.validity().is_valid() {
            bail!("expired or not valid yet")
        }
        if cert.public_key().raw != key.subject_public_key_info() {
            bail!("certificate and key do not belong together")
        }
        let subject = cert.subject();
        let values = |attrs: Vec<&x509_parser::x509::AttributeTypeAndValue>| {
            attrs
                .into_iter()
                .map(|a| a.as_str().map(String::from))
                .collect::<Result<Vec<_>, _>>()
        };
        if values(subject.iter_common_name().collect())? != [cn]
            || values(subject.iter_organization().collect())? != [o]
        {
            bail!("unexpected subject '{}'", subject)
        }
        Ok(())
    }

    /// The sorted subject alternative names of the certificate.
    fn sans(cert: &X509Certificate) -> Result<Vec<String>> {
        let mut sans = vec![];
        if let Some(ext) = cert.subject_alternative_name()? {
            for name in &ext.value.general_names {
                sans.push(match name {
                    GeneralName::DNSName(dns) => (*dns).to_string(),
                    GeneralName::IPAddress(ip) => match ip.len() {
                        4 => Ipv4Addr::from(<[u8; 4]>::try_from(*ip)?).to_string(),
                        16 => Ipv6Addr::from(<[u8; 16]>::try_from(*ip)?).to_string(),
                        _ => bail!("invalid IP address SAN"),
                    },
                    other => bail!("unexpected SAN {:?}", other),
                });
            }
        }
        sans.sort();
        sans.dedup();
        Ok(sans)
    }

    /// The hosts as they appear in the subject alternative names, sorted.
    fn normalize_hosts(hosts: &[String]) -> Vec<String> {
        let mut hosts: Vec<String> = hosts
            .iter()
            .map(|h| {
                h.parse::<IpAddr>()
                    .map_or_else(|_| h.clone(), |ip| ip.to_string())
            })
            .collect();
        hosts.sort();
        hosts.dedup();
        hosts
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

    /// Write the PEM encoded key and certificate of the identity. Only the
    /// owner can read the key. Both get replaced atomically, and a crash in
    /// between leaves a mismatching pair behind, which gets regenerated.
    fn write(identity: &Identity, cert: &str, key: &KeyPair) -> Result<()> {
        write_private(identity.key(), key.serialize_pem().as_bytes())?;
        write_file(identity.cert(), cert.as_bytes())
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
    use std::os::unix::fs::PermissionsExt;
    use x509_parser::extensions::ParsedExtension;

    /// Run the check against the parsed certificate of the identity.
    fn with_cert<T>(identity: &Identity, check: impl FnOnce(&X509Certificate) -> T) -> Result<T> {
        let pem = Pki::read_cert(identity.cert())?;
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
        with_cert(identity, Pki::sans)?
    }

    fn cas(pki: &Pki) -> Vec<&Identity> {
        vec![
            pki.ca(),
            pki.etcd_ca(),
            pki.front_proxy_ca(),
            pki.kubelet_ca(),
        ]
    }

    /// All leaf certificates with their CA and usage.
    fn leaves(pki: &Pki) -> Vec<(&Identity, &Identity, Usage)> {
        let mut leaves = vec![
            (pki.admin(), pki.ca(), Usage::Client),
            (pki.apiserver(), pki.ca(), Usage::Server),
            (pki.apiserver_etcd_client(), pki.etcd_ca(), Usage::Client),
            (pki.apiserver_kubelet_client(), pki.ca(), Usage::Client),
            (pki.controller_manager(), pki.ca(), Usage::Client),
            (pki.etcd(), pki.etcd_ca(), Usage::Peer),
            (pki.etcd_peer(), pki.etcd_ca(), Usage::Peer),
            (
                pki.front_proxy_client(),
                pki.front_proxy_ca(),
                Usage::Client,
            ),
            (pki.proxy(), pki.ca(), Usage::Client),
            (pki.scheduler(), pki.ca(), Usage::Client),
        ];
        leaves.extend(pki.kubelets().iter().map(|k| (k, pki.ca(), Usage::Client)));
        leaves.extend(
            pki.kubelet_servers()
                .iter()
                .map(|k| (k, pki.kubelet_ca(), Usage::Server)),
        );
        leaves
    }

    /// The contents of all files in the PKI directory.
    fn files(pki: &Pki) -> Result<Vec<(PathBuf, Vec<u8>)>> {
        let dir = pki.ca().cert().parent().context("no PKI dir")?;
        let mut files = fs::read_dir(dir)?
            .map(|e| {
                let path = e?.path();
                Ok((path.clone(), fs::read(&path)?))
            })
            .collect::<Result<Vec<_>>>()?;
        files.sort();
        Ok(files)
    }

    #[test]
    fn new_success() -> Result<()> {
        let c = test_config()?;
        let n = test_network()?;
        let pki = Pki::new(&c, &n)?;
        let mode = fs::metadata(c.root().join(Pki::DIR))?.permissions().mode();
        assert_eq!(mode & 0o777, 0o700);
        // Each identity has its own files
        assert_eq!(
            files(&pki)?.len(),
            2 * (cas(&pki).len() + leaves(&pki).len()) + 2
        );
        Ok(())
    }

    #[test]
    fn certificates_have_own_sans() -> Result<()> {
        let c = test_config()?;
        let n = test_network()?;
        let pki = Pki::new(&c, &n)?;

        let mut apiserver = vec![
            n.api()?.to_string(),
            "127.0.0.1".into(),
            // The advertised address in root mode
            "10.10.0.1".into(),
            n.hostname().into(),
            "kubernetes".into(),
            "kubernetes.default".into(),
            "kubernetes.default.svc".into(),
            "kubernetes.default.svc.cluster.local".into(),
        ];
        apiserver.sort();
        assert_eq!(sans(pki.apiserver())?, apiserver);

        assert_eq!(sans(pki.etcd())?, ["127.0.0.1", "localhost"]);
        assert_eq!(sans(pki.etcd_peer())?, ["127.0.0.1", "localhost"]);

        // The gateway of the bridge, which is the node IP in root mode,
        // without the addresses of the host, which may change
        let expected = vec![
            "127.0.0.1".to_string(),
            n.hostname().into(),
            "10.10.0.1".into(),
        ];
        assert_eq!(
            sans(&pki.kubelet_servers()[0])?,
            Pki::normalize_hosts(&expected)
        );

        for (identity, _, usage) in leaves(&pki) {
            if usage == Usage::Client {
                assert!(sans(identity)?.is_empty(), "{} has SANs", identity.name());
            }
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
            (pki.etcd_ca(), "etcd-ca", "etcd-ca"),
            (pki.front_proxy_ca(), "front-proxy-ca", "front-proxy-ca"),
            (pki.kubelet_ca(), "node-serving-ca", "node-serving-ca"),
            (pki.admin(), "admin", "system:masters"),
            (pki.apiserver(), "kubernetes", "kubernetes"),
            (
                pki.apiserver_etcd_client(),
                "kube-apiserver-etcd-client",
                "kubernetes",
            ),
            (
                pki.apiserver_kubelet_client(),
                "kube-apiserver-kubelet-client",
                "kubernetes",
            ),
            (
                pki.controller_manager(),
                "system:kube-controller-manager",
                "system:kube-controller-manager",
            ),
            (pki.etcd(), "etcd", "etcd"),
            (pki.etcd_peer(), "etcd", "etcd"),
            (
                pki.front_proxy_client(),
                "front-proxy-client",
                "front-proxy-client",
            ),
            (&pki.kubelets()[0], &node, "system:nodes"),
            (&pki.kubelet_servers()[0], &node, "system:nodes"),
            (pki.proxy(), "system:kube-proxy", "system:node-proxier"),
            (
                pki.scheduler(),
                "system:kube-scheduler",
                "system:kube-scheduler",
            ),
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

        for ca in cas(&pki) {
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

        for (leaf, _, usage) in leaves(&pki) {
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
                assert_eq!(eku.server_auth, usage.server(), "{}", leaf.name());
                assert_eq!(eku.client_auth, usage.client(), "{}", leaf.name());
                assert!(eku.other.is_empty() && !eku.any && !eku.code_signing);
                Ok(())
            })??;
        }

        // Only the etcd certificates are both server and client
        let peers: Vec<_> = leaves(&pki)
            .into_iter()
            .filter(|(_, _, usage)| *usage == Usage::Peer)
            .map(|(identity, _, _)| identity.name().to_string())
            .collect();
        assert_eq!(peers, ["etcd", "etcd-peer"]);
        Ok(())
    }

    #[test]
    fn certificates_signed_by_ca() -> Result<()> {
        let c = test_config()?;
        let n = test_network()?;
        let pki = Pki::new(&c, &n)?;

        let cas = cas(&pki)
            .into_iter()
            .map(|ca| Ok((ca.name().to_string(), Pki::read_cert(ca.cert())?)))
            .collect::<Result<Vec<_>>>()?;
        for (_, pem) in &cas {
            let cert = pem.parse_x509()?;
            assert_eq!(cert.issuer(), cert.subject());
            cert.verify_signature(None)?;
            let (aki, ski) = key_ids(&cert);
            assert!(aki.is_none() && ski.is_some());
        }

        for (leaf, issuer, _) in leaves(&pki) {
            for (name, pem) in &cas {
                let ca = pem.parse_x509()?;
                with_cert(leaf, |cert| -> Result<()> {
                    if name == issuer.name() {
                        assert_eq!(cert.issuer(), ca.subject(), "{}", leaf.name());
                        cert.verify_signature(Some(ca.public_key()))?;
                        let (aki, ski) = key_ids(cert);
                        assert!(aki.is_some() && ski.is_some(), "{}", leaf.name());
                        assert_eq!(aki, key_ids(&ca).1, "{}", leaf.name());
                    } else {
                        // No other CA trusts the certificate
                        assert!(
                            cert.verify_signature(Some(ca.public_key())).is_err(),
                            "{} trusted by {}",
                            leaf.name(),
                            name
                        );
                    }
                    Ok(())
                })??;
            }
        }
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
        let mut identities: Vec<_> = cas(&pki).into_iter().map(|i| (i, CA_VALIDITY)).collect();
        identities.extend(leaves(&pki).into_iter().map(|(i, _, _)| (i, CERT_VALIDITY)));
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
        let mut identities: Vec<_> = leaves(&pki).into_iter().map(|(i, _, _)| i).collect();
        identities.extend(cas(&pki));
        for identity in identities {
            let mode = fs::metadata(identity.key())?.permissions().mode();
            assert_eq!(mode & 0o777, 0o600, "{}", identity.key().display());
            let key = Pki::read_key(identity.key())?;
            // The key belongs to the certificate
            let public_key = with_cert(identity, |cert| cert.public_key().raw.to_vec())?;
            assert_eq!(key.subject_public_key_info(), public_key);
        }

        let sa = pki.service_account();
        let mode = fs::metadata(sa.key())?.permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
        let key = Pki::read_key(sa.key())?;
        assert_eq!(fs::read_to_string(sa.public_key())?, key.public_key_pem());
        Ok(())
    }

    #[test]
    fn kubelet_certificate_does_not_collide() -> Result<()> {
        let c = test_config()?;
        let n = test_network()?;
        let pki = Pki::new(&c, &n)?;
        assert_eq!(
            pki.kubelets()[0].name(),
            format!("kubelet-{}", n.hostname())
        );
        assert_eq!(
            pki.kubelet_servers()[0].name(),
            format!("kubelet-{}-serving", n.hostname())
        );
        assert!(!KUBELET_CA_NAME.starts_with("kubelet-"));
        Ok(())
    }

    #[test]
    fn new_reuses_valid_files() -> Result<()> {
        let c = test_config()?;
        let n = test_network()?;
        let before = files(&Pki::new(&c, &n)?)?;
        assert_eq!(files(&Pki::new(&c, &n)?)?, before);
        Ok(())
    }

    #[test]
    fn new_removes_obsolete_files() -> Result<()> {
        let c = test_config()?;
        let n = test_network()?;
        let pki = Pki::new(&c, &n)?;
        let before = files(&pki)?;
        let dir = pki.ca().cert().parent().context("no PKI dir")?;
        for file in OBSOLETE_FILES {
            fs::write(dir.join(file), "")?;
        }
        assert_eq!(files(&Pki::new(&c, &n)?)?, before);
        Ok(())
    }

    #[test]
    fn new_regenerates_invalid_leaves() -> Result<()> {
        let c = test_config()?;
        let n = test_network()?;
        let pki = Pki::new(&c, &n)?;
        let ca = fs::read(pki.ca().cert())?;
        let admin = fs::read(pki.admin().cert())?;
        let sa = fs::read(pki.service_account().key())?;

        // A truncated key and a certificate of another identity, see
        // new_regenerates_etcd_server_only_certificate for an outdated one
        fs::write(pki.admin().key(), "")?;
        fs::copy(pki.proxy().cert(), pki.scheduler().cert())?;

        let pki = Pki::new(&c, &n)?;
        assert_ne!(fs::read(pki.admin().cert())?, admin);
        Pki::read_key(pki.admin().key())?;
        assert_eq!(
            with_cert(pki.scheduler(), |cert| cert.subject().to_string())?,
            "O=system:kube-scheduler, OU=kubernetes, CN=system:kube-scheduler"
        );
        assert_eq!(fs::read(pki.ca().cert())?, ca);
        assert_eq!(fs::read(pki.service_account().key())?, sa);

        // Everything is valid now
        let before = files(&pki)?;
        assert_eq!(files(&Pki::new(&c, &n)?)?, before);
        Ok(())
    }

    #[test]
    fn apiserver_certificate_gets_advertise_address() -> Result<()> {
        // A run directory of a rootless cluster, whose API server does not
        // advertise a fixed address, gets reused in root mode
        let mut c = test_config()?;
        c.set_rootless(true);
        let pki = Pki::new(&c, &Network::new(&c)?)?;
        assert!(!sans(pki.apiserver())?.contains(&"10.10.0.1".to_string()));

        c.set_rootless(false);
        let pki = Pki::new(&c, &Network::new(&c)?)?;
        assert!(sans(pki.apiserver())?.contains(&"10.10.0.1".to_string()));
        Ok(())
    }

    #[test]
    fn check_detects_changed_hosts() -> Result<()> {
        let c = test_config()?;
        let n = test_network()?;
        let pki = Pki::new(&c, &n)?;
        let ca = Pki::load_ca(pki.etcd_ca())?;
        let mut leaf = Leaf {
            identity: pki.etcd(),
            organization: ETCD_NAME,
            usage: Usage::Peer,
            hosts: vec!["localhost".into(), "127.0.0.1".into()],
            ca: &ca,
        };
        leaf.check()?;
        leaf.hosts.push("10.0.0.1".into());
        assert!(leaf.check().is_err());
        leaf.hosts.pop();
        leaf.usage = Usage::Server;
        assert!(leaf.check().is_err());
        leaf.usage = Usage::Peer;

        // Signed by another CA
        let other = Pki::load_ca(pki.ca())?;
        leaf.ca = &other;
        assert!(leaf.check().is_err());
        Ok(())
    }

    #[test]
    fn new_regenerates_etcd_server_only_certificate() -> Result<()> {
        // Older versions issued the etcd certificate for server
        // authentication only, which breaks the gRPC gateway of etcd
        let c = test_config()?;
        let n = test_network()?;
        let pki = Pki::new(&c, &n)?;
        let ca = Pki::load_ca(pki.etcd_ca())?;
        let old = Leaf {
            identity: pki.etcd(),
            organization: ETCD_NAME,
            usage: Usage::Server,
            hosts: vec!["127.0.0.1".into(), "localhost".into()],
            ca: &ca,
        };
        old.generate()?;
        old.check()?;
        let admin = fs::read(pki.admin().cert())?;

        let pki = Pki::new(&c, &n)?;
        with_cert(pki.etcd(), |cert| -> Result<()> {
            let eku = cert.extended_key_usage()?.context("no EKU")?.value;
            assert!(eku.server_auth && eku.client_auth);
            Ok(())
        })??;
        // The other certificates are reused
        assert_eq!(fs::read(pki.admin().cert())?, admin);
        Ok(())
    }

    #[test]
    fn new_regenerates_without_etcd_data() -> Result<()> {
        let c = test_config()?;
        let n = test_network()?;
        let pki = Pki::new(&c, &n)?;
        let admin = fs::read(pki.admin().cert())?;
        fs::write(pki.ca().cert(), "invalid")?;
        fs::remove_file(pki.service_account().key())?;

        // All certificates of the new CA get regenerated
        let pki = Pki::new(&c, &n)?;
        assert_ne!(fs::read(pki.admin().cert())?, admin);
        Pki::load_ca(pki.ca())?;
        Pki::read_key(pki.service_account().key())?;
        Ok(())
    }

    #[test]
    fn new_refuses_regeneration_with_etcd_data() -> Result<()> {
        let c = test_config()?;
        let n = test_network()?;
        let pki = Pki::new(&c, &n)?;
        fs::create_dir_all(Etcd::data_dir(&c).join("member"))?;

        // Other CAs can be regenerated
        fs::remove_file(pki.etcd_ca().key())?;
        fs::remove_file(pki.kubelet_ca().cert())?;
        let pki = Pki::new(&c, &n)?;

        let ca = fs::read(pki.ca().key())?;
        fs::write(pki.ca().key(), "")?;
        let err = Pki::new(&c, &n).err().context("no error")?;
        assert!(format!("{:#}", err).contains("etcd data"), "{:#}", err);
        fs::write(pki.ca().key(), ca)?;

        fs::remove_file(pki.service_account().key())?;
        let err = Pki::new(&c, &n).err().context("no error")?;
        assert!(
            format!("{:#}", err).contains("ServiceAccount key"),
            "{:#}",
            err
        );
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
