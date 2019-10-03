use crate::{config::Config, kubeconfig::KubeConfig, Kubernix};
use failure::{bail, Fallible};
use log::{debug, info};
use std::{
    fs::{self, create_dir_all},
    process::Command,
};

pub struct CoreDNS;

impl CoreDNS {
    pub fn apply(config: &Config, kubeconfig: &KubeConfig) -> Fallible<()> {
        info!("Deploying CoreDNS");

        let dir = config.root().join("coredns");
        create_dir_all(&dir)?;

        let yml = format!(include_str!("assets/coredns.yml"), Kubernix::dns(config)?);
        let yml_file = dir.join("coredns.yml");
        fs::write(&yml_file, yml)?;

        let output = Command::new("kubectl")
            .arg("apply")
            .arg(format!("--kubeconfig={}", kubeconfig.admin.display()))
            .arg("-f")
            .arg(yml_file)
            .output()?;
        if !output.status.success() {
            debug!(
                "kubectl apply stdout: {}",
                String::from_utf8(output.stdout)?
            );
            debug!(
                "kubectl apply stderr: {}",
                String::from_utf8(output.stderr)?
            );
            bail!("kubectl apply command failed");
        }

        info!("CoreDNS deployed");
        Ok(())
    }
}
