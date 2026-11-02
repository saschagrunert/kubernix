//! Kubectl command wrapper.
//!
//! Provides a typed interface around the `kubectl` binary for applying
//! manifests and waiting for pod readiness.

use crate::{
    process::{self, READINESS_TIMEOUT},
    signals,
};
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

    /// Run a kubectl command like `execute`, but terminate it as soon as
    /// `cancel` returns true.
    fn execute_cancelable(
        &self,
        args: &[&str],
        cancel: impl Fn() -> bool + Sync,
    ) -> Result<Output> {
        let output = process::run_cancelable(
            self.command(args)
                .stdout(Stdio::piped())
                .stderr(Stdio::piped()),
            None,
            cancel,
        )
        .context("Unable to run kubectl")?;
        Self::check_output(args, output)
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut cmd = process::command("kubectl");
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
                || shutdown.load(Ordering::Relaxed),
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
    /// includes the post start hooks like the RBAC bootstrap roles. Stops
    /// on termination signals and if the startup got canceled.
    pub fn wait_api_ready(&self) -> Result<()> {
        debug!("Waiting for the API server /readyz endpoint");
        let cancel = startup_canceled();
        poll(
            Duration::from_secs(READINESS_TIMEOUT),
            Duration::from_secs(1),
            &cancel,
            || {
                let output = self.execute_cancelable(
                    &["get", "--raw=/readyz", "--request-timeout=5s"],
                    &cancel,
                )?;
                Ok(String::from_utf8_lossy(&output.stdout).trim() == "ok")
            },
        )
        .context("API server did not become ready")?;
        debug!("API server /readyz is ok");
        Ok(())
    }
}

/// Returns a check whether a termination signal arrived or the startup of
/// the component on the current thread got canceled. It captures the
/// cancel flag of the current thread, so it works on other threads too,
/// like the one of `run_cancelable` which terminates the command.
fn startup_canceled() -> impl Fn() -> bool + Sync {
    let flag = process::cancel_flag();
    move || signals::requested() || flag.as_ref().is_some_and(|f| f.load(Ordering::SeqCst))
}

/// Run `check` every `interval` until it returns true, `timeout` elapsed or
/// `cancel` returns true, which fails with `process::Canceled`, so that the
/// boot phase does not count it as a failure. Errors of `check` are treated
/// as not ready yet, the last one is part of the timeout error.
fn poll<C, F>(timeout: Duration, interval: Duration, cancel: C, mut check: F) -> Result<()>
where
    C: Fn() -> bool,
    F: FnMut() -> Result<bool>,
{
    let start = Instant::now();
    loop {
        if cancel() {
            return Err(process::Canceled.into());
        }
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
        let next = Instant::now() + interval;
        while Instant::now() < next && !cancel() {
            sleep(SHUTDOWN_POLL_INTERVAL.min(interval));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{path::PathBuf, sync::Arc};

    #[test]
    fn execute_success() -> Result<()> {
        let k = Kubectl::new(&PathBuf::from(""));
        k.execute(&[])?;
        Ok(())
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
        poll(
            Duration::from_secs(5),
            Duration::ZERO,
            || false,
            || {
                calls += 1;
                match calls {
                    1 => bail!("connection refused"),
                    2 => Ok(false),
                    _ => Ok(true),
                }
            },
        )?;
        assert_eq!(calls, 3);
        Ok(())
    }

    #[test]
    fn poll_timeout_contains_last_error() {
        let err = poll(
            Duration::ZERO,
            Duration::ZERO,
            || false,
            || bail!("not healthy"),
        )
        .unwrap_err();
        let msg = format!("{:#}", err);
        assert!(msg.contains("Timed out after 0s"));
        assert!(msg.contains("not healthy"));
    }

    #[test]
    fn poll_timeout_without_error() {
        let err = poll(Duration::ZERO, Duration::ZERO, || false, || Ok(false)).unwrap_err();
        assert_eq!(err.to_string(), "Timed out after 0s");
    }

    #[test]
    fn poll_stops_on_cancel() {
        let start = Instant::now();
        let err = poll(
            Duration::from_secs(30),
            Duration::from_secs(10),
            || start.elapsed() > Duration::from_millis(200),
            || Ok(false),
        )
        .unwrap_err();
        assert!(err.downcast_ref::<process::Canceled>().is_some(), "{err:#}");
        assert!(start.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn startup_canceled_on_other_threads() {
        let flag = Arc::new(AtomicBool::new(false));
        process::set_cancel(Arc::clone(&flag));
        let cancel = startup_canceled();
        std::thread::scope(|s| {
            assert!(!s.spawn(&cancel).join().unwrap());
            flag.store(true, Ordering::SeqCst);
            assert!(s.spawn(&cancel).join().unwrap());
        });
        flag.store(false, Ordering::SeqCst);
    }

    #[test]
    fn kubeconfig_path() {
        let path = PathBuf::from("/tmp/test.kubeconfig");
        let k = Kubectl::new(&path);
        assert_eq!(k.kubeconfig(), path);
    }
}
