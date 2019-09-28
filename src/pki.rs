use crate::Config;
use failure::{bail, format_err, Fallible};
use log::{debug, info};
use serde_json::{json, to_string_pretty};
use std::{
    fs::{self, create_dir_all},
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

#[derive(Default)]
pub struct Pki {
    pub kubelet_cert: PathBuf,
    pub kubelet_key: PathBuf,
    pub apiserver_cert: PathBuf,
    pub apiserver_key: PathBuf,
    pub proxy_cert: PathBuf,
    pub proxy_key: PathBuf,
    pub controller_manager_cert: PathBuf,
    pub controller_manager_key: PathBuf,
    pub scheduler_cert: PathBuf,
    pub scheduler_key: PathBuf,
    pub service_account_cert: PathBuf,
    pub service_account_key: PathBuf,
    pub admin_cert: PathBuf,
    pub admin_key: PathBuf,
    pub ca_cert: PathBuf,
    pub ca_key: PathBuf,
    ca_config: PathBuf,
    ip: String,
    hostname: String,
}

impl Pki {
    pub fn new(config: &Config, ip: &str, hostname: &str) -> Fallible<Pki> {
        info!("Generating certificates");

        // Create the target dir
        let pki_dir = &config.root.join(&config.pki.dir);
        create_dir_all(pki_dir)?;

        let mut pki = Pki::default();
        pki.ip = ip.to_owned();
        pki.hostname = hostname.to_owned();
        pki.setup_ca(pki_dir)?;
        pki.setup_kubelet(pki_dir)?;
        pki.setup_admin(pki_dir)?;
        pki.setup_controller_manager(pki_dir)?;
        pki.setup_proxy(pki_dir)?;
        pki.setup_scheduler(pki_dir)?;
        pki.setup_apiserver(pki_dir)?;
        pki.setup_service_account(pki_dir)?;

        Ok(pki)
    }

    fn setup_ca(&mut self, dir: &Path) -> Fallible<()> {
        const PREFIX: &str = "ca";
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
        let status = Command::new("cfssljson")
            .arg("-bare")
            .arg(dir.join(PREFIX))
            .stdin(pipe)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()?;
        if !status.success() {
            bail!("CA certificate generation failed");
        }
        debug!("CA certificates created");
        self.ca_cert = dir.join(format!("{}.pem", PREFIX));
        self.ca_key = dir.join(format!("{}-key.pem", PREFIX));
        Ok(())
    }

    fn setup_kubelet(&mut self, dir: &Path) -> Fallible<()> {
        let prefix = format!("system:node:{}", self.hostname);
        let csr_file = dir.join("node-csr.json");
        self.write_csr(&prefix, "system:nodes", &csr_file)?;

        let (c, k) = self.generate(dir, &prefix, &csr_file)?;
        self.kubelet_cert = c;
        self.kubelet_key = k;
        Ok(())
    }

    fn setup_admin(&mut self, dir: &Path) -> Fallible<()> {
        const PREFIX: &str = "admin";
        let csr_file = dir.join("admin-csr.json");
        self.write_csr(PREFIX, "system:masters", &csr_file)?;

        let (c, k) = self.generate(dir, PREFIX, &csr_file)?;
        self.admin_cert = c;
        self.admin_key = k;
        Ok(())
    }

    fn setup_controller_manager(&mut self, dir: &Path) -> Fallible<()> {
        const PREFIX: &str = "kube-controller-manager";
        const CN: &str = "system:kube-controller-manager";
        let csr_file = dir.join("kube-controller-manager-csr.json");
        self.write_csr(CN, CN, &csr_file)?;

        let (c, k) = self.generate(dir, PREFIX, &csr_file)?;
        self.controller_manager_cert = c;
        self.controller_manager_key = k;
        Ok(())
    }

    fn setup_proxy(&mut self, dir: &Path) -> Fallible<()> {
        const PREFIX: &str = "kube-proxy";
        let csr_file = dir.join("admin-csr.json");
        self.write_csr("system:kube-proxy", "system:node-proxier", &csr_file)?;

        let (c, k) = self.generate(dir, PREFIX, &csr_file)?;
        self.proxy_cert = c;
        self.proxy_key = k;
        Ok(())
    }

    fn setup_scheduler(&mut self, dir: &Path) -> Fallible<()> {
        const PREFIX: &str = "kube-scheduler";
        let csr_file = dir.join("kube-scheduler-csr.json");
        const CN: &str = "system:kube-scheduler";
        self.write_csr(CN, CN, &csr_file)?;

        let (c, k) = self.generate(dir, PREFIX, &csr_file)?;
        self.scheduler_cert = c;
        self.scheduler_key = k;
        Ok(())
    }

    fn setup_apiserver(&mut self, dir: &Path) -> Fallible<()> {
        const PREFIX: &str = "kubernetes";
        let csr_file = dir.join("kubernetes-csr.json");
        self.write_csr(PREFIX, PREFIX, &csr_file)?;

        let (c, k) = self.generate(dir, PREFIX, &csr_file)?;
        self.apiserver_cert = c;
        self.apiserver_key = k;
        Ok(())
    }

    fn setup_service_account(&mut self, dir: &Path) -> Fallible<()> {
        const PREFIX: &str = "service-account";
        let csr_file = dir.join("service-account-csr.json");
        self.write_csr("service-accounts", "Kubernetes", &csr_file)?;

        let (c, k) = self.generate(dir, PREFIX, &csr_file)?;
        self.service_account_cert = c;
        self.service_account_key = k;
        Ok(())
    }

    fn generate(
        &mut self,
        dir: &Path,
        name: &str,
        csr: &Path,
    ) -> Fallible<(PathBuf, PathBuf)> {
        debug!("Creating certificate for {}", name);
        let hostnames = &[
            &self.ip,
            "127.0.0.1",
            &self.hostname,
            "kubernetes",
            "kubernetes.default",
            "kubernetes.default.svc",
            "kubernetes.default.svc.cluster",
            "kubernetes.svc.cluster.local",
        ];
        let mut cfssl = Command::new("cfssl")
            .arg("gencert")
            .arg(format!("-ca={}", self.ca_cert.display()))
            .arg(format!("-ca-key={}", self.ca_key.display()))
            .arg(format!("-config={}", self.ca_config.display()))
            .arg("-profile=kubernetes")
            .arg(format!("-hostname={}", hostnames.join(",")))
            .arg(csr)
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()?;

        let pipe = cfssl
            .stdout
            .take()
            .ok_or_else(|| format_err!("unable to get stdout"))?;
        let status = Command::new("cfssljson")
            .arg("-bare")
            .arg(dir.join(name))
            .stdin(pipe)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()?;
        if !status.success() {
            bail!("cfssl command failed");
        }
        debug!("Certificate created for {}", name);

        Ok((
            dir.join(format!("{}.pem", name)),
            dir.join(format!("{}-key.pem", name)),
        ))
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
                    "usages": ["signing", "key encipherment", "server auth", "client auth"],
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
