use crate::Config;
use failure::{bail, format_err, Fallible};
use ipnetwork::IpNetwork;
use log::{debug, info};
use serde_json::{json, to_string_pretty};
use std::{
    fs::{self, create_dir_all},
    net::Ipv4Addr,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

#[derive(Default)]
pub struct Pki {
    pub kubelet: Pair,
    pub apiserver: Pair,
    pub proxy: Pair,
    pub controller_manager: Pair,
    pub scheduler: Pair,
    pub service_account: Pair,
    pub admin: Pair,
    pub ca: Pair,
    ca_config: PathBuf,
    hostnames: String,
}

#[derive(Default)]
pub struct Pair {
    cert: PathBuf,
    key: PathBuf,
}

impl Pair {
    pub fn new(dir: &Path, name: &str) -> Pair {
        let cert = dir.join(format!("{}.pem", name));
        let key = dir.join(format!("{}-key.pem", name));
        Pair { cert, key }
    }

    pub fn cert(&self) -> &Path {
        &self.cert
    }

    pub fn key(&self) -> &Path {
        &self.key
    }
}

impl Pki {
    pub fn new(config: &Config, ip: &str, hostname: &str) -> Fallible<Pki> {
        info!("Generating certificates");

        // Create the target dir
        let pki_dir = &config.root.join(&config.pki.dir);
        create_dir_all(pki_dir)?;

        let mut pki = Pki::default();

        let service_addr = match config.kube.service_cidr {
            IpNetwork::V4(n) => n.nth(1).ok_or_else(|| {
                format_err!(
                    "Unable to retrieve first IP from service CIDR: {}",
                    config.kube.service_cidr
                )
            })?,
            _ => Ipv4Addr::LOCALHOST,
        };

        let hostnames = &[
            ip,
            &service_addr.to_string(),
            &Ipv4Addr::LOCALHOST.to_string(),
            hostname,
            "kubernetes",
            "kubernetes.default",
            "kubernetes.default.svc",
            "kubernetes.default.svc.cluster",
            "kubernetes.svc.cluster.local",
        ];
        pki.hostnames = hostnames.join(",");

        pki.setup_ca(pki_dir)?;
        pki.setup_kubelet(pki_dir, hostname)?;
        pki.setup_admin(pki_dir)?;
        pki.setup_controller_manager(pki_dir)?;
        pki.setup_proxy(pki_dir)?;
        pki.setup_scheduler(pki_dir)?;
        pki.setup_apiserver(pki_dir)?;
        pki.setup_service_account(pki_dir)?;

        Ok(pki)
    }

    fn setup_ca(&mut self, dir: &Path) -> Fallible<()> {
        const NAME: &str = "ca";
        debug!("Creating CA certificates");
        self.write_ca_config(dir)?;
        const CN: &str = "Kubernetes";
        let csr = dir.join("ca-csr.json");
        self.write_csr(CN, CN, &csr)?;

        let mut cfssl = Command::new("cfssl")
            .arg("gencert")
            .arg("-initca")
            .arg(csr)
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()?;

        let pipe = cfssl
            .stdout
            .take()
            .ok_or_else(|| format_err!("unable to get stdout"))?;
        let output = Command::new("cfssljson")
            .arg("-bare")
            .arg(dir.join(NAME))
            .stdin(pipe)
            .output()?;
        if !output.status.success() {
            debug!("cfssl/json stdout: {}", String::from_utf8(output.stdout)?);
            debug!("cfssl/json stderr: {}", String::from_utf8(output.stderr)?);
            bail!("CA certificate generation failed");
        }
        debug!("CA certificates created");
        self.ca = Pair::new(dir, NAME);
        Ok(())
    }

    fn setup_kubelet(&mut self, dir: &Path, hostname: &str) -> Fallible<()> {
        let name = format!("system:node:{}", hostname);
        let csr_file = dir.join("node-csr.json");
        self.write_csr(&name, "system:nodes", &csr_file)?;

        self.kubelet = self.generate(dir, hostname, &csr_file)?;
        Ok(())
    }

    fn setup_admin(&mut self, dir: &Path) -> Fallible<()> {
        const NAME: &str = "admin";
        let csr_file = dir.join("admin-csr.json");
        self.write_csr(NAME, "system:masters", &csr_file)?;

        self.admin = self.generate(dir, NAME, &csr_file)?;
        Ok(())
    }

    fn setup_controller_manager(&mut self, dir: &Path) -> Fallible<()> {
        const NAME: &str = "kube-controller-manager";
        const CN: &str = "system:kube-controller-manager";
        let csr_file = dir.join("kube-controller-manager-csr.json");
        self.write_csr(CN, CN, &csr_file)?;

        self.controller_manager = self.generate(dir, NAME, &csr_file)?;
        Ok(())
    }

    fn setup_proxy(&mut self, dir: &Path) -> Fallible<()> {
        const NAME: &str = "kube-proxy";
        let csr_file = dir.join("admin-csr.json");
        self.write_csr("system:kube-proxy", "system:node-proxier", &csr_file)?;

        self.proxy = self.generate(dir, NAME, &csr_file)?;
        Ok(())
    }

    fn setup_scheduler(&mut self, dir: &Path) -> Fallible<()> {
        const NAME: &str = "kube-scheduler";
        let csr_file = dir.join("kube-scheduler-csr.json");
        const CN: &str = "system:kube-scheduler";
        self.write_csr(CN, CN, &csr_file)?;

        self.scheduler = self.generate(dir, NAME, &csr_file)?;
        Ok(())
    }

    fn setup_apiserver(&mut self, dir: &Path) -> Fallible<()> {
        const NAME: &str = "kubernetes";
        let csr_file = dir.join("kubernetes-csr.json");
        self.write_csr(NAME, NAME, &csr_file)?;

        self.apiserver = self.generate(dir, NAME, &csr_file)?;
        Ok(())
    }

    fn setup_service_account(&mut self, dir: &Path) -> Fallible<()> {
        const NAME: &str = "service-account";
        let csr_file = dir.join("service-account-csr.json");
        self.write_csr("service-accounts", "Kubernetes", &csr_file)?;

        self.service_account = self.generate(dir, NAME, &csr_file)?;
        Ok(())
    }

    fn generate(&mut self, dir: &Path, name: &str, csr: &Path) -> Fallible<Pair> {
        debug!("Creating certificate for {}", name);

        let mut cfssl = Command::new("cfssl")
            .arg("gencert")
            .arg(format!("-ca={}", self.ca.cert().display()))
            .arg(format!("-ca-key={}", self.ca.key().display()))
            .arg(format!("-config={}", self.ca_config.display()))
            .arg("-profile=kubernetes")
            .arg(format!("-hostname={}", self.hostnames))
            .arg(csr)
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()?;

        let pipe = cfssl
            .stdout
            .take()
            .ok_or_else(|| format_err!("unable to get stdout"))?;
        let output = Command::new("cfssljson")
            .arg("-bare")
            .arg(dir.join(name))
            .stdin(pipe)
            .output()?;
        if !output.status.success() {
            debug!("cfssl/json stdout: {}", String::from_utf8(output.stdout)?);
            debug!("cfssl/json stderr: {}", String::from_utf8(output.stderr)?);
            bail!("cfssl command failed");
        }
        debug!("Certificate created for {}", name);

        Ok(Pair::new(dir, name))
    }

    fn write_csr(&self, cn: &str, o: &str, dest: &Path) -> Fallible<()> {
        let csr = json!({
          "CN": cn,
          "key": {
            "algo": "rsa",
            "size": 2048
          },
          "names": [
            {
              "C": "US",
              "L": "Portland",
              "O": o,
              "OU": "Kubernetes",
              "ST": "Oregon"
            }
          ]
        });
        fs::write(dest, to_string_pretty(&csr)?)?;
        Ok(())
    }

    fn write_ca_config(&mut self, dir: &Path) -> Fallible<()> {
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
        self.ca_config = dest;
        Ok(())
    }
}
