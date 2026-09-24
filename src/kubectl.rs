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

    /// Wait until the API server reports readiness via `/readyz`, which
    /// includes the post start hooks like the RBAC bootstrap roles.
    pub fn wait_api_ready(&self) -> Result<()> {
        debug!("Waiting for the API server /readyz endpoint");
        poll(
            Duration::from_secs(READINESS_TIMEOUT),
            Duration::from_secs(1),
            || {
                let output = self.execute(&["get", "--raw=/readyz"])?;
                Ok(String::from_utf8_lossy(&output.stdout).trim() == "ok")
            },
        )
        .context("API server did not become ready")?;
        debug!("API server /readyz is ok");
        Ok(())
    }
}

/// Run `check` every `interval` until it returns true or `timeout` elapsed.
/// Errors of `check` are treated as not ready yet, the last one is part of
/// the timeout error.
fn poll<F>(timeout: Duration, interval: Duration, mut check: F) -> Result<()>
where
    F: FnMut() -> Result<bool>,
{
    let start = Instant::now();
    loop {
        let last_error = match check() {
            Ok(true) => return Ok(()),
            Ok(false) => None,
            Err(e) => {
                trace!("Not ready yet: {:#}", e);
                Some(e)
            }
        };
        if start.elapsed() >= timeout {
            let msg = format!("Timed out after {}s", timeout.as_secs());
            return Err(match last_error {
                Some(e) => e.context(msg),
                None => anyhow::anyhow!(msg),
            });
        }
        sleep(interval);
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
    fn poll_success_after_retries() -> Result<()> {
        let mut calls = 0;
        poll(Duration::from_secs(5), Duration::ZERO, || {
            calls += 1;
            match calls {
                1 => bail!("connection refused"),
                2 => Ok(false),
                _ => Ok(true),
            }
        })?;
        assert_eq!(calls, 3);
        Ok(())
    }

    #[test]
    fn poll_timeout_contains_last_error() {
        let err = poll(Duration::ZERO, Duration::ZERO, || bail!("not healthy")).unwrap_err();
        let msg = format!("{:#}", err);
        assert!(msg.contains("Timed out after 0s"));
        assert!(msg.contains("not healthy"));
    }

    #[test]
    fn poll_timeout_without_error() {
        let err = poll(Duration::ZERO, Duration::ZERO, || Ok(false)).unwrap_err();
        assert_eq!(err.to_string(), "Timed out after 0s");
    }

    #[test]
    fn kubeconfig_path() {
        let path = PathBuf::from("/tmp/test.kubeconfig");
        let k = Kubectl::new(&path);
        assert_eq!(k.kubeconfig(), path);
    }
}
