//! Kubectl command wrapper.
//!
//! Provides a typed interface around the `kubectl` binary for applying
//! manifests, configuring kubeconfig files, and waiting for pod readiness.

use crate::process::READINESS_TIMEOUT;
use anyhow::{Context, Result, bail};
use log::{debug, trace};
use std::{
    path::{Path, PathBuf},
    process::{Command, Output},
    thread::sleep,
    time::{Duration, Instant},
};

/// Wraps the `kubectl` binary with a fixed kubeconfig path.
pub struct Kubectl {
    kubeconfig: PathBuf,
}

impl Kubectl {
    /// The kubeconfig file path used for all kubectl invocations.
    pub fn kubeconfig(&self) -> &Path {
        &self.kubeconfig
    }

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
            .output()
            .context("Unable to run kubectl")?;
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

    /// Wait for all pods matching a label to be ready
    pub fn wait_ready(&self, name: &str) -> Result<()> {
        debug!("Waiting for {} to be ready", name);
        let now = Instant::now();
        while now.elapsed().as_secs() < READINESS_TIMEOUT {
            let output = self.execute(&[
                "wait",
                "--for=condition=Ready",
                "pod",
                "-n=kube-system",
                &format!("-l=k8s-app={}", name),
                "--timeout=5s",
            ]);
            match output {
                Ok(_) => {
                    debug!("{} ready", name);
                    return Ok(());
                }
                Err(e) => {
                    debug!(
                        "{} not ready yet ({}/{}s): {}",
                        name,
                        now.elapsed().as_secs(),
                        READINESS_TIMEOUT,
                        e,
                    );
                }
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

    #[test]
    fn kubeconfig_path() {
        let path = PathBuf::from("/tmp/test.kubeconfig");
        let k = Kubectl::new(&path);
        assert_eq!(k.kubeconfig(), path);
    }
}
