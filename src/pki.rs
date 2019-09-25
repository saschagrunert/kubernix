use crate::{Config, ASSETS_DIR};
use failure::{bail, format_err, Fallible};
use log::{debug, info};
use std::{
    fs::create_dir_all,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

#[derive(Default)]
pub struct Pki {
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
    pub ca: PathBuf,
    ip: String,
}

impl Pki {
    pub fn new(config: &Config, ip: &str) -> Fallible<Pki> {
        info!("Generating certificates");

        // Create the target dir
        let pki_dir = &config.root.join(&config.pki.dir);
        create_dir_all(pki_dir)?;

        let mut pki = Pki::default();
        pki.ip = ip.to_owned();
        pki.setup_ca(pki_dir)?;
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
        let mut cfssl = Command::new("cfssl")
            .arg("gencert")
            .arg("-initca")
            .arg(Path::new(ASSETS_DIR).join("ca-csr.json"))
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
        self.ca = dir.join(format!("{}.pem", PREFIX));
        Ok(())
    }

    fn setup_admin(&mut self, dir: &Path) -> Fallible<()> {
        const PREFIX: &str = "admin";
        let (c, k) = self.generate(dir, PREFIX, "assets/admin-csr.json")?;
        self.admin_cert = c;
        self.admin_key = k;
        Ok(())
    }

    fn setup_controller_manager(&mut self, dir: &Path) -> Fallible<()> {
        const PREFIX: &str = "kube-controller-manager";
        let (c, k) = self.generate(
            dir,
            PREFIX,
            "assets/kube-controller-manager-csr.json",
        )?;
        self.controller_manager_cert = c;
        self.controller_manager_key = k;
        Ok(())
    }

    fn setup_proxy(&mut self, dir: &Path) -> Fallible<()> {
        const PREFIX: &str = "kube-proxy";
        let (c, k) =
            self.generate(dir, PREFIX, "assets/kube-proxy-csr.json")?;
        self.proxy_cert = c;
        self.proxy_key = k;
        Ok(())
    }

    fn setup_scheduler(&mut self, dir: &Path) -> Fallible<()> {
        const PREFIX: &str = "kube-scheduler";
        let (c, k) =
            self.generate(dir, PREFIX, "assets/kube-scheduler-csr.json")?;
        self.scheduler_cert = c;
        self.scheduler_key = k;
        Ok(())
    }

    fn setup_apiserver(&mut self, dir: &Path) -> Fallible<()> {
        const PREFIX: &str = "kubernetes";
        let (c, k) =
            self.generate(dir, PREFIX, "assets/kubernetes-csr.json")?;
        self.apiserver_cert = c;
        self.apiserver_key = k;
        Ok(())
    }

    fn setup_service_account(&mut self, dir: &Path) -> Fallible<()> {
        const PREFIX: &str = "service-account";
        let (c, k) =
            self.generate(dir, PREFIX, "assets/service-account-csr.json")?;
        self.service_account_cert = c;
        self.service_account_key = k;
        Ok(())
    }

    fn generate(
        &mut self,
        dir: &Path,
        name: &str,
        csr: &str,
    ) -> Fallible<(PathBuf, PathBuf)> {
        debug!("Creating certificate for {}", name);
        let hostnames = &[
            &self.ip,
            "127.0.0.1",
            "kubernetes",
            "kubernetes.default",
            "kubernetes.default.svc",
            "kubernetes.default.svc.cluster",
            "kubernetes.svc.cluster.local",
        ];
        let mut cfssl = Command::new("cfssl")
            .arg("gencert")
            .arg(format!("-ca={}", dir.join("ca.pem").display()))
            .arg(format!("-ca-key={}", dir.join("ca-key.pem").display()))
            .arg("-config=assets/ca-config.json")
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
}
