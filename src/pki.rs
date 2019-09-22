use crate::Config;
use failure::Fallible;
use log::debug;
use std::process::{Command, Stdio};

pub struct Pki;

impl Pki {
    pub fn setup(config: &Config) -> Fallible<()> {
        Self::setup_ca()?;
        Self::setup_admin()?;
        Self::setup_controller_manager()?;
        Self::setup_proxy()?;
        Self::setup_scheduler()?;
        Self::setup_apiserver()?;
        Self::setup_service_account()
    }

    fn setup_ca() -> Fallible<()> {
        debug!("Creating CA certificates");
        let mut cfssl = Command::new("cfssl")
            .arg("gencert")
            .arg("-initca")
            .arg("assets/ca-csr.json")
            .stdout(Stdio::piped())
            .spawn()?;

        let pipe = cfssl.stdout.take().unwrap();
        Command::new("cfssljson")
            .arg("-bare")
            .arg("ca")
            .stdin(pipe)
            .output()?;
        debug!("CA certificates created");
        Ok(())
    }

    fn setup_admin() -> Fallible<()> {
        Self::generate("admin", "assets/admin-csr.json", "")
    }

    fn setup_controller_manager() -> Fallible<()> {
        Self::generate(
            "kube-controller-manager",
            "assets/kube-controller-manager-csr.json",
            "",
        )
    }

    fn setup_proxy() -> Fallible<()> {
        Self::generate("kube-proxy", "assets/kube-proxy-csr.json", "")
    }

    fn setup_scheduler() -> Fallible<()> {
        Self::generate("kube-scheduler", "assets/kube-scheduler-csr.json", "")
    }

    fn setup_apiserver() -> Fallible<()> {
        Self::generate("kubernetes", "assets/kubernetes-csr.json",
            "-hostname=10.32.0.1,10.240.0.10,10.240.0.11,10.240.0.12,127.0.0.1,kubernetes,kubernetes.default,kubernetes.default.svc,kubernetes.default.svc.cluster,kubernetes.svc.cluster.local")
    }

    fn setup_service_account() -> Fallible<()> {
        Self::generate("service-account", "assets/service-account-csr.json", "")
    }

    fn generate(name: &str, csr: &str, additional_arg: &str) -> Fallible<()> {
        debug!("Creating certificate for {}", name);
        let mut cfssl = Command::new("cfssl")
            .arg("gencert")
            .arg("-ca=ca.pem")
            .arg("-ca-key=ca-key.pem")
            .arg("-config=assets/ca-config.json")
            .arg("-profile=kubernetes")
            .arg(additional_arg)
            .arg(csr)
            .stdout(Stdio::piped())
            .spawn()?;

        let pipe = cfssl.stdout.take().unwrap();
        Command::new("cfssljson")
            .arg("-bare")
            .arg(name)
            .stdin(pipe)
            .output()?;
        debug!("Certificate created for {}", name);
        Ok(())
    }
}
