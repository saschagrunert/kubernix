use crate::Config;
use failure::Fallible;
use log::debug;
use std::{
    fs::create_dir_all,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

const ASSETS_DIR: &str = "assets";

#[derive(Default)]
pub struct Pki {
    pub apiserver_cert: PathBuf,
    pub apiserver_key: PathBuf,
    pub ca: PathBuf,
}

impl Pki {
    pub fn setup(config: &Config) -> Fallible<Pki> {
        // Create the target dir
        let pki_dir = Path::new(&config.pki.dir);
        create_dir_all(pki_dir)?;

        let mut pki = Pki::default();
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
            .spawn()?;

        let pipe = cfssl.stdout.take().unwrap();
        Command::new("cfssljson")
            .arg("-bare")
            .arg(dir.join(PREFIX))
            .stdin(pipe)
            .output()?;
        debug!("CA certificates created");
        self.ca = dir.join(format!("{}.pem", PREFIX));
        Ok(())
    }

    fn setup_admin(&mut self, dir: &Path) -> Fallible<()> {
        self.generate(dir, "admin", "assets/admin-csr.json")
    }

    fn setup_controller_manager(&mut self, dir: &Path) -> Fallible<()> {
        self.generate(
            dir,
            "kube-controller-manager",
            "assets/kube-controller-manager-csr.json",
        )
    }

    fn setup_proxy(&mut self, dir: &Path) -> Fallible<()> {
        self.generate(dir, "kube-proxy", "assets/kube-proxy-csr.json")
    }

    fn setup_scheduler(&mut self, dir: &Path) -> Fallible<()> {
        self.generate(dir, "kube-scheduler", "assets/kube-scheduler-csr.json")
    }

    fn setup_apiserver(&mut self, dir: &Path) -> Fallible<()> {
        const PREFIX: &str = "kubernetes";
        self.generate(dir, PREFIX, "assets/kubernetes-csr.json")?;
        self.apiserver_cert = dir.join(format!("{}.pem", PREFIX));
        self.apiserver_key = dir.join(format!("{}-key.pem", PREFIX));
        Ok(())
    }

    fn setup_service_account(&mut self, dir: &Path) -> Fallible<()> {
        self.generate(dir, "service-account", "assets/service-account-csr.json")
    }

    fn generate(&mut self, dir: &Path, name: &str, csr: &str) -> Fallible<()> {
        debug!("Creating certificate for {}", name);
        let hostnames = &[
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
            .spawn()?;

        let pipe = cfssl.stdout.take().unwrap();
        Command::new("cfssljson")
            .arg("-bare")
            .arg(dir.join(name))
            .stdin(pipe)
            .output()?;
        debug!("Certificate created for {}", name);
        Ok(())
    }
}
