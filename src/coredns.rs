use crate::{config::Config, kubeconfig::KubeConfig};
use failure::{bail, Fallible};
use log::info;
use std::{
    fs::{self, create_dir_all},
    process::{Command, Stdio},
};

pub struct CoreDNS;

impl CoreDNS {
    pub fn apply(config: &Config, kubeconfig: &KubeConfig) -> Fallible<()> {
        info!("Deploying CoreDNS");

        let dir = config.root.join("coredns");
        create_dir_all(&dir)?;

        let yml = format!(include_str!("assets/coredns.yml"), config.kube.cluster_dns);
        let yml_file = dir.join("coredns.yml");
        fs::write(&yml_file, yml)?;

        let status = Command::new("kubectl")
            .arg("apply")
            .arg(format!("--kubeconfig={}", kubeconfig.admin.display()))
            .arg("-f")
            .arg(yml_file)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()?;
        if !status.success() {
            bail!("kubectl apply command failed");
        }

        info!("CoreDNS deployed");
        Ok(())
    }
}
