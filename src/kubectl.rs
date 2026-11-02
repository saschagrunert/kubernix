//! Kubectl command wrapper.
//!
//! Provides a typed interface around the `kubectl` binary for applying
//! manifests and waiting for pod readiness.

use crate::process::READINESS_TIMEOUT;
use anyhow::{Context, Result, bail};
use log::{debug, trace};
use std::{
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
    sync::atomic::{AtomicBool, Ordering},
    thread::sleep,
    time::{Duration, Instant},
};

/// Interval for checking the shutdown flag while waiting.
const SHUTDOWN_POLL_INTERVAL: Duration = Duration::from_millis(100);

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
        let output = self
            .command(args)
            .output()
            .context("Unable to run kubectl")?;
        Self::check_output(args, output)
    }

    /// Run a kubectl command like `execute`, but kill it as soon as
    /// `shutdown` is set.
    fn execute_cancelable(&self, args: &[&str], shutdown: &AtomicBool) -> Result<Output> {
        let output =
            run_cancelable(self.command(args), shutdown).context("Unable to run kubectl")?;
        Self::check_output(args, output)
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut cmd = Command::new("kubectl");
        cmd.args(args).arg("--kubeconfig").arg(&self.kubeconfig);
        cmd
    }

    fn check_output(args: &[&str], output: Output) -> Result<Output> {
        if !output.status.success() {
            trace!("kubectl args: {:?}", args);
            debug!("kubectl output: {:?}", output);
            bail!("kubectl command failed");
        }
        Ok(output)
    }

    /// Run kubectl apply
    pub fn apply(&self, file: &Path) -> Result<()> {
        let file_arg = file.display().to_string();
        let args = &["apply", "-f", &file_arg];
        self.execute(args)?;
        Ok(())
    }

    /// Wait for all pods matching a label to be ready, or until `shutdown`
    /// is set.
    pub fn wait_ready(&self, name: &str, shutdown: &AtomicBool) -> Result<()> {
        debug!("Waiting for {} to be ready", name);
        let now = Instant::now();
        while now.elapsed().as_secs() < READINESS_TIMEOUT {
            if shutdown.load(Ordering::Relaxed) {
                bail!("Shutdown requested while waiting for {}", name)
            }
            let output = self.execute_cancelable(
                &[
                    "wait",
                    "--for=condition=Ready",
                    "pod",
                    "-n=kube-system",
                    &format!("-l=k8s-app={}", name),
                    "--timeout=5s",
                    "--request-timeout=5s",
                ],
                shutdown,
            );
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
            let retry = Instant::now() + Duration::from_secs(2);
            while Instant::now() < retry && !shutdown.load(Ordering::Relaxed) {
                sleep(SHUTDOWN_POLL_INTERVAL);
            }
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

/// Run the command and collect its output, kill it if `shutdown` gets set.
/// The output must fit into the pipe buffers, which is the case for short
/// running commands like `kubectl wait`.
fn run_cancelable(mut cmd: Command, shutdown: &AtomicBool) -> Result<Output> {
    let mut child = cmd.stdout(Stdio::piped()).stderr(Stdio::piped()).spawn()?;
    while child.try_wait()?.is_none() {
        if shutdown.load(Ordering::Relaxed) {
            let _ = child.kill();
            let _ = child.wait();
            bail!("Shutdown requested")
        }
        sleep(SHUTDOWN_POLL_INTERVAL);
    }
    Ok(child.wait_with_output()?)
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
    use std::{path::PathBuf, sync::Arc, thread};

    #[test]
    fn execute_success() -> Result<()> {
        let k = Kubectl::new(&PathBuf::from(""));
        k.execute(&[])?;
        Ok(())
    }

    #[test]
    fn run_cancelable_output() -> Result<()> {
        let mut cmd = Command::new("echo");
        cmd.arg("hello");
        let output = run_cancelable(cmd, &AtomicBool::new(false))?;
        assert!(output.status.success());
        assert_eq!(String::from_utf8_lossy(&output.stdout), "hello\n");
        Ok(())
    }

    #[test]
    fn run_cancelable_stops_on_shutdown() {
        let shutdown = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&shutdown);
        let setter = thread::spawn(move || {
            sleep(Duration::from_millis(200));
            flag.store(true, Ordering::Relaxed);
        });
        let start = Instant::now();
        let mut cmd = Command::new("sleep");
        cmd.arg("30");
        let err = run_cancelable(cmd, &shutdown).unwrap_err();
        assert_eq!(err.to_string(), "Shutdown requested");
        assert!(start.elapsed() < Duration::from_secs(5));
        setter.join().unwrap();
    }

    #[test]
    fn wait_ready_returns_on_shutdown() {
        let k = Kubectl::new(&PathBuf::from(""));
        let start = Instant::now();
        let err = k.wait_ready("coredns", &AtomicBool::new(true)).unwrap_err();
        assert!(err.to_string().contains("Shutdown requested"));
        assert!(start.elapsed() < Duration::from_secs(1));
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
