use crate::kubeconfig::KubeConfig;
use failure::{bail, Fallible};
use log::debug;
use std::{path::Path, process::Command};

pub struct Kubectl;

impl Kubectl {
    pub fn apply(kubeconfig: &KubeConfig, file: &Path) -> Fallible<()> {
        let output = Command::new("kubectl")
            .arg("apply")
            .arg(format!("--kubeconfig={}", kubeconfig.admin().display()))
            .arg("-f")
            .arg(file)
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
        Ok(())
    }
}
