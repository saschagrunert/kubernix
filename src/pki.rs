use crate::{Config, network::Network, node::Node};
use anyhow::{Context, Result, bail};
use log::{debug, info};
use serde_json::{json, to_string_pretty};
use std::{
    fs::{self, create_dir_all},
    net::Ipv4Addr,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

pub struct Pki {
    admin: Identity,
    apiserver: Identity,
    ca: Identity,
    controller_manager: Identity,
    kubelets: Vec<Identity>,
    proxy: Identity,
    scheduler: Identity,
    service_account: Identity,
}

impl Pki {
    pub fn admin(&self) -> &Identity {
        &self.admin
    }

    pub fn apiserver(&self) -> &Identity {
        &self.apiserver
    }

    pub fn ca(&self) -> &Identity {
        &self.ca
    }

    pub fn controller_manager(&self) -> &Identity {
        &self.controller_manager
    }

    pub fn kubelets(&self) -> &Vec<Identity> {
        &self.kubelets
    }

    pub fn proxy(&self) -> &Identity {
        &self.proxy
    }

    pub fn scheduler(&self) -> &Identity {
        &self.scheduler
    }

    pub fn service_account(&self) -> &Identity {
        &self.service_account
    }
}

pub struct Identity {
    name: String,
    user: String,
    cert: PathBuf,
    key: PathBuf,
}

impl Identity {
    pub fn name(&self) -> &String {
        &self.name
    }

    pub fn user(&self) -> &String {
        &self.user
    }

    pub fn cert(&self) -> &PathBuf {
        &self.cert
    }

    pub fn key(&self) -> &PathBuf {
        &self.key
    }

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
    ca_config: PathBuf,
    dir: &'a Path,
    hostnames: &'a str,
}

impl<'a> PkiConfig<'a> {
    fn ca(&self) -> &Identity {
        self.ca
    }

    fn ca_config(&self) -> &PathBuf {
        &self.ca_config
    }

    fn dir(&self) -> &Path {
        self.dir
    }

    fn hostnames(&self) -> &str {
        self.hostnames
    }
}

const ADMIN_NAME: &str = "admin";
const APISERVER_NAME: &str = "kubernetes";
const CA_NAME: &str = "ca";
const CONTROLLER_MANAGER_NAME: &str = "kube-controller-manager";
const CONTROLLER_MANAGER_USER: &str = "system:kube-controller-manager";
const PROXY_NAME: &str = "kube-proxy";
const PROXY_USER: &str = "system:kube-proxy";
const SCHEDULER_NAME: &str = "kube-scheduler";
const SCHEDULER_USER: &str = "system:kube-scheduler";
const SERVICE_ACCOUNT_NAME: &str = "service-account";

impl Pki {
    pub fn new(config: &Config, network: &Network) -> Result<Pki> {
        let dir = &config.root().join("pki");
        let nodes = (0..config.nodes())
            .map(|n| Node::name(config, network, n))
            .collect::<Vec<String>>();

        // Create the CA only if necessary
        if dir.exists() {
            info!("PKI directory already exists, skipping generation");

            let kubelets = if config.multi_node() {
                // Multiple nodes get identified via their node name
                nodes
                    .iter()
                    .map(|n| Identity::new(dir, n, &Self::node_user(n)))
                    .collect()
            } else {
                // Single node gets identified via its hostname
                vec![Identity::new(
                    dir,
                    network.hostname(),
                    &Self::node_user(network.hostname()),
                )]
            };

            Ok(Pki {
                admin: Identity::new(dir, ADMIN_NAME, ADMIN_NAME),
                apiserver: Identity::new(dir, APISERVER_NAME, APISERVER_NAME),
                ca: Identity::new(dir, CA_NAME, CA_NAME),
                controller_manager: Identity::new(
                    dir,
                    CONTROLLER_MANAGER_NAME,
                    CONTROLLER_MANAGER_USER,
                ),
                kubelets,
                proxy: Identity::new(dir, PROXY_NAME, PROXY_USER),
                scheduler: Identity::new(dir, SCHEDULER_NAME, SCHEDULER_USER),
                service_account: Identity::new(dir, SERVICE_ACCOUNT_NAME, SERVICE_ACCOUNT_NAME),
            })
        } else {
            info!("Generating certificates");
            create_dir_all(dir)?;
            let ca_config = Self::write_ca_config(dir)?;
            let ca = Self::setup_ca(dir)?;

            let mut hostnames = vec![
                network.api()?.to_string(),
                Ipv4Addr::LOCALHOST.to_string(),
                network.hostname().into(),
                "kubernetes".into(),
                "kubernetes.default".into(),
                "kubernetes.default.svc".into(),
                "kubernetes.default.svc.cluster".into(),
                "kubernetes.svc.cluster.local".into(),
            ];
            hostnames.extend(nodes.clone());

            let pki_config = &PkiConfig {
                dir,
                ca: &ca,
                ca_config,
                hostnames: &hostnames.join(","),
            };

            let kubelets = if config.multi_node() {
                // Multiple nodes get identified via their node name
                nodes
                    .iter()
                    .map(|n| Self::setup_kubelet(pki_config, n))
                    .collect::<Result<Vec<_>, _>>()?
            } else {
                // Single node gets identified via its hostname
                vec![Self::setup_kubelet(pki_config, network.hostname())?]
            };

            Ok(Pki {
                admin: Self::setup_admin(pki_config)?,
                apiserver: Self::setup_apiserver(pki_config)?,
                controller_manager: Self::setup_controller_manager(pki_config)?,
                kubelets,
                proxy: Self::setup_proxy(pki_config)?,
                scheduler: Self::setup_scheduler(pki_config)?,
                service_account: Self::setup_service_account(pki_config)?,
                ca,
            })
        }
    }

    fn setup_ca(dir: &Path) -> Result<Identity> {
        debug!("Creating CA certificates");
        const CN: &str = "kubernetes";
        let csr = dir.join("ca-csr.json");
        Self::write_csr(CN, CN, &csr)?;

        let mut cfssl = Command::new("cfssl")
            .arg("gencert")
            .arg("-initca")
            .arg(csr)
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()?;

        let pipe = cfssl.stdout.take().context("unable to get stdout")?;
        let output = Command::new("cfssljson")
            .arg("-bare")
            .arg(dir.join(CA_NAME))
            .stdin(pipe)
            .output()?;
        if !output.status.success() {
            debug!("cfssl/json: {:?}", output);
            bail!("CA certificate generation failed");
        }
        debug!("CA certificates created");
        Ok(Identity::new(dir, CA_NAME, CA_NAME))
    }

    fn setup_kubelet(pki_config: &PkiConfig, node: &str) -> Result<Identity> {
        let user = Self::node_user(node);
        let csr_file = pki_config.dir().join(format!("{}-csr.json", node));
        Self::write_csr(&user, "system:nodes", &csr_file)?;
        Self::generate(pki_config, node, &csr_file, &user)
    }

    fn setup_admin(pki_config: &PkiConfig) -> Result<Identity> {
        let csr_file = pki_config.dir().join("admin-csr.json");
        Self::write_csr(ADMIN_NAME, "system:masters", &csr_file)?;
        Self::generate(pki_config, ADMIN_NAME, &csr_file, ADMIN_NAME)
    }

    fn setup_controller_manager(pki_config: &PkiConfig) -> Result<Identity> {
        let csr_file = pki_config.dir().join("kube-controller-manager-csr.json");
        Self::write_csr(CONTROLLER_MANAGER_USER, CONTROLLER_MANAGER_USER, &csr_file)?;
        Self::generate(
            pki_config,
            CONTROLLER_MANAGER_NAME,
            &csr_file,
            CONTROLLER_MANAGER_USER,
        )
    }

    fn setup_proxy(pki_config: &PkiConfig) -> Result<Identity> {
        let csr_file = pki_config.dir().join("kube-proxy-csr.json");
        Self::write_csr("system:kube-proxy", "system:node-proxier", &csr_file)?;
        Self::generate(pki_config, PROXY_NAME, &csr_file, PROXY_USER)
    }

    fn setup_scheduler(pki_config: &PkiConfig) -> Result<Identity> {
        let csr_file = pki_config.dir().join("kube-scheduler-csr.json");
        Self::write_csr(SCHEDULER_USER, SCHEDULER_USER, &csr_file)?;
        Self::generate(pki_config, SCHEDULER_NAME, &csr_file, SCHEDULER_USER)
    }

    fn setup_apiserver(pki_config: &PkiConfig) -> Result<Identity> {
        let csr_file = pki_config.dir().join("kubernetes-csr.json");
        Self::write_csr(APISERVER_NAME, APISERVER_NAME, &csr_file)?;
        Self::generate(pki_config, APISERVER_NAME, &csr_file, APISERVER_NAME)
    }

    fn setup_service_account(pki_config: &PkiConfig) -> Result<Identity> {
        let csr_file = pki_config.dir().join("service-account-csr.json");
        Self::write_csr("service-accounts", "kubernetes", &csr_file)?;
        Self::generate(
            pki_config,
            SERVICE_ACCOUNT_NAME,
            &csr_file,
            SERVICE_ACCOUNT_NAME,
        )
    }

    fn generate(pki_config: &PkiConfig, name: &str, csr: &Path, user: &str) -> Result<Identity> {
        debug!("Creating certificate for {}", name);

        let mut cfssl = Command::new("cfssl")
            .arg("gencert")
            .arg(format!("-ca={}", pki_config.ca().cert().display()))
            .arg(format!("-ca-key={}", pki_config.ca().key().display()))
            .arg(format!("-config={}", pki_config.ca_config().display()))
            .arg("-profile=kubernetes")
            .arg(format!("-hostname={}", pki_config.hostnames()))
            .arg(csr)
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()?;

        let pipe = cfssl.stdout.take().context("unable to get stdout")?;
        let output = Command::new("cfssljson")
            .arg("-bare")
            .arg(pki_config.dir().join(name))
            .stdin(pipe)
            .output()?;
        if !output.status.success() {
            debug!("cfssl/json: {:?}", output.stdout);
            bail!("cfssl command failed");
        }
        debug!("Certificate created for {}", name);

        Ok(Identity::new(pki_config.dir(), name, user))
    }

    fn write_csr(cn: &str, o: &str, dest: &Path) -> Result<()> {
        let csr = json!({
            "CN": cn,
            "key": {
                "algo": "rsa",
                "size": 2048
            },
            "names": [{
                "O": o,
                "OU": "kubernetes",
            }]
        });
        fs::write(dest, to_string_pretty(&csr)?)?;
        Ok(())
    }

    fn write_ca_config(dir: &Path) -> Result<PathBuf> {
        let cfg = json!({
            "signing": {
                "default": {
                    "expiry": "8760h"
                },
                "profiles": {
                    "kubernetes": {
                        "usages": [
                            "signing",
                            "key encipherment",
                            "server auth",
                            "client auth"
                        ],
                        "expiry": "8760h"
                    }
                }
            }
        });
        let dest = dir.join("ca-config.json");
        fs::write(&dest, to_string_pretty(&cfg)?)?;
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
        Pki::new(&c, &n)?;
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
