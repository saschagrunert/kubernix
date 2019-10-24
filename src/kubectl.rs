use anyhow::{bail, Result};
use getset::Getters;
use log::{debug, trace};
use std::{
    path::{Path, PathBuf},
    process::{Command, Output},
    thread::sleep,
    time::{Duration, Instant},
};

#[derive(Getters)]
pub struct Kubectl {
    #[get = "pub"]
    kubeconfig: PathBuf,
}

impl Kubectl {
    /// Create a new kubectl client for the provided kubeconfig
    pub fn new(kubeconfig: &Path) -> Self {
        Self {
            kubeconfig: kubeconfig.into(),
        }
    }

    /// Run a generic kubectl command
    pub fn execute(&self, args: &[&str]) -> Result<Output> {
        let output = Command::new("kubectl")
            .args(args)
            .arg("--kubeconfig")
            .arg(&self.kubeconfig)
            .output()?;
        if !output.status.success() {
            trace!("kubectl args: {:?}", args);
            debug!("kubectl output: {:?}", output);
            bail!("kubectl command failed");
        }
        Ok(output)
    }

    /// Run kubectl config
    pub fn config(&self, args: &[&str]) -> Result<()> {
        let mut final_args = vec!["config"];
        final_args.extend(args);
        self.execute(&final_args)?;
        Ok(())
    }

    /// Run kubectl apply
    pub fn apply(&self, file: &Path) -> Result<()> {
        let file_arg = file.display().to_string();
        let args = &["apply", "-f", &file_arg];
        self.execute(args)?;
        Ok(())
    }

    /// Wait for a pod to be ready
    pub fn wait_ready(&self, name: &str) -> Result<()> {
        debug!("Waiting for {} to be ready", name);
        const TIMEOUT: u64 = 60;
        let now = Instant::now();
        while now.elapsed().as_secs() < TIMEOUT {
            let output = self.execute(&[
                "get",
                "pods",
                "-n=kube-system",
                &format!("-l=k8s-app={}", name),
                "--no-headers",
            ])?;
            let stdout = String::from_utf8(output.stdout)?;
            if let Some(status) = stdout.split_whitespace().nth(1) {
                debug!(
                    "{} status: {} ({}/{}s)",
                    name,
                    status,
                    now.elapsed().as_secs(),
                    TIMEOUT,
                );
                if stdout.contains("1/1") {
                    debug!("{} ready", name);
                    return Ok(());
                }
            } else {
                debug!(
                    "{} status not available ({}/{}s)",
                    name,
                    now.elapsed().as_secs(),
                    TIMEOUT,
                )
            }
            sleep(Duration::from_secs(2));
        }
        bail!("Unable to wait for {} pod", name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn execute_success() -> Result<()> {
        let k = Kubectl::new(&PathBuf::from(""));
        k.execute(&[])?;
        Ok(())
    }
}
