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
            match Self::error_line(&String::from_utf8_lossy(&output.stderr)) {
                Some(line) => bail!("kubectl command failed: {}", line),
                None => bail!("kubectl command failed"),
            }
        }
        Ok(output)
    }

    /// The line of the kubectl output which names the error: the first one
    /// starting like an error, otherwise the last non-empty one, since
    /// kubectl may end with usage hints.
    fn error_line(stderr: &str) -> Option<&str> {
        let mut lines = stderr.lines().map(str::trim).filter(|l| !l.is_empty());
        lines
            .clone()
            .find(|l| l.starts_with("error:") || l.starts_with("Error from server"))
            .or_else(|| lines.next_back())
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
        let cancel = || shutdown.load(Ordering::Relaxed);
        let label = format!("-l=k8s-app={}", name);
        wait_for(
            name,
            Duration::from_secs(READINESS_TIMEOUT),
            Duration::from_secs(2),
            cancel,
            || false,
            || {
                self.execute_cancelable(
                    &[
                        "wait",
                        "--for=condition=Ready",
                        "pod",
                        "-n=kube-system",
                        &label,
                        "--timeout=5s",
                        "--request-timeout=5s",
                    ],
                    cancel,
                )
                .map(|_| true)
            },
        )
        .map_err(|e| {
            let msg = if e.is::<process::Canceled>() {
                format!("Shutdown requested while waiting for {}", name)
            } else {
                format!("Unable to wait for {} pod", name)
            };
            e.context(msg)
        })?;
        debug!("{} ready", name);
        Ok(())
    }

    /// Wait until the API server reports readiness via `/readyz`, which
    /// includes the post start hooks like the RBAC bootstrap roles. Stops
    /// on termination signals and if the startup got canceled.
    pub fn wait_api_ready(&self) -> Result<()> {
        debug!("Waiting for the API server /readyz endpoint");
        let cancel = startup_canceled();
        wait_for(
            "the API server",
            Duration::from_secs(READINESS_TIMEOUT),
            Duration::from_secs(1),
            &cancel,
            || false,
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

    /// Wait until the controller manager created the `default` service
    /// account, which every pod of the `default` namespace needs. Without
    /// it, creating pods right after the startup fails. Stops on
    /// termination signals and once `died` reports a died component.
    pub fn wait_default_service_account(&self, died: impl FnMut() -> bool) -> Result<()> {
        debug!("Waiting for the default service account");
        let cancel = signals::requested;
        wait_for(
            "the default service account",
            Duration::from_secs(READINESS_TIMEOUT),
            Duration::from_secs(1),
            cancel,
            died,
            || {
                self.execute_cancelable(
                    &[
                        "get",
                        "serviceaccount",
                        "default",
                        "--namespace=default",
                        "--output=name",
                        "--request-timeout=5s",
                    ],
                    cancel,
                )
                .map(|_| true)
            },
        )
        .map_err(|e| {
            if e.is::<process::Canceled>() {
                // Only a termination signal cancels this wait
                signals::check().err().unwrap_or(e)
            } else {
                e.context("The default service account did not get created")
            }
        })?;
        debug!("The default service account exists");
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

/// Run `check` every `interval` until it returns true, which means `what`
/// is ready, or `timeout` elapsed. It fails with `process::Canceled` if
/// `cancel` returns true or `check` got canceled, so that the boot phase
/// does not count it as a failure, and fails right away if `died` returns
/// true. Errors of `check` are treated as not ready yet and get logged,
/// the last one is part of the failure.
fn wait_for<C, D, F>(
    what: &str,
    timeout: Duration,
    interval: Duration,
    cancel: C,
    mut died: D,
    mut check: F,
) -> Result<()>
where
    C: Fn() -> bool,
    D: FnMut() -> bool,
    F: FnMut() -> Result<bool>,
{
    let failure = |last_error: Option<anyhow::Error>, msg: String| match last_error {
        Some(e) => e.context(msg),
        None => anyhow::anyhow!(msg),
    };
    let start = Instant::now();
    let mut last_error = None;
    loop {
        if cancel() {
            return Err(process::Canceled.into());
        }
        if died() {
            return Err(failure(
                last_error,
                "A cluster component died unexpectedly".into(),
            ));
        }
        match check() {
            Ok(true) => return Ok(()),
            Ok(false) => last_error = None,
            Err(e) if e.is::<process::Canceled>() => return Err(e),
            Err(e) => {
                debug!(
                    "{} not ready yet ({}/{}s): {:#}",
                    what,
                    start.elapsed().as_secs(),
                    timeout.as_secs(),
                    e,
                );
                last_error = Some(e);
            }
        }
        if start.elapsed() >= timeout {
            let msg = format!("Timed out after {}s", timeout.as_secs());
            return Err(failure(last_error, msg));
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
    fn error_line_prefers_errors() {
        assert_eq!(
            Kubectl::error_line("error: unknown flag: --x\nSee 'kubectl get --help' for usage.\n"),
            Some("error: unknown flag: --x")
        );
        assert_eq!(
            Kubectl::error_line(
                "Error from server (NotFound): a\nError from server (NotFound): b\n"
            ),
            Some("Error from server (NotFound): a")
        );
        assert_eq!(
            Kubectl::error_line("warning\nsomething\n\n"),
            Some("something")
        );
        assert_eq!(Kubectl::error_line("\n"), None);
    }

    #[test]
    fn wait_for_fails_once_a_component_died() {
        let checks = std::cell::Cell::new(0);
        let err = wait_for(
            "test",
            Duration::from_secs(60),
            Duration::from_millis(1),
            || false,
            || checks.get() >= 2,
            || {
                checks.set(checks.get() + 1);
                bail!("not found")
            },
        )
        .unwrap_err();
        assert_eq!(
            format!("{:#}", err),
            "A cluster component died unexpectedly: not found"
        );
        assert_eq!(checks.get(), 2);
    }

    #[test]
    fn wait_for_stops_on_canceled_check() {
        let mut checks = 0;
        let err = wait_for(
            "test",
            Duration::from_secs(60),
            Duration::from_millis(1),
            || false,
            || false,
            || {
                checks += 1;
                Err(process::Canceled.into())
            },
        )
        .unwrap_err();
        assert!(err.is::<process::Canceled>());
        assert_eq!(checks, 1);
    }

    #[test]
    fn check_output_names_the_cause() {
        use std::os::unix::process::ExitStatusExt;
        let output = |stderr: &str| Output {
            status: std::process::ExitStatus::from_raw(256),
            stdout: vec![],
            stderr: stderr.as_bytes().to_vec(),
        };
        let err = Kubectl::check_output(&[], output("Error from server (NotFound)\nhint\n\n"))
            .unwrap_err();
        assert_eq!(
            err.to_string(),
            "kubectl command failed: Error from server (NotFound)"
        );
        let err = Kubectl::check_output(&[], output("")).unwrap_err();
        assert_eq!(err.to_string(), "kubectl command failed");
    }

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
    fn wait_for_success_after_retries() -> Result<()> {
        let mut calls = 0;
        wait_for(
            "test",
            Duration::from_secs(5),
            Duration::ZERO,
            || false,
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
    fn wait_for_timeout_contains_last_error() {
        let err = wait_for(
            "test",
            Duration::ZERO,
            Duration::ZERO,
            || false,
            || false,
            || bail!("not healthy"),
        )
        .unwrap_err();
        let msg = format!("{:#}", err);
        assert!(msg.contains("Timed out after 0s"));
        assert!(msg.contains("not healthy"));
    }

    #[test]
    fn wait_for_timeout_without_error() {
        let err = wait_for(
            "test",
            Duration::ZERO,
            Duration::ZERO,
            || false,
            || false,
            || Ok(false),
        )
        .unwrap_err();
        assert_eq!(err.to_string(), "Timed out after 0s");
    }

    #[test]
    fn wait_for_stops_on_cancel() {
        let start = Instant::now();
        let err = wait_for(
            "test",
            Duration::from_secs(30),
            Duration::from_secs(10),
            || start.elapsed() > Duration::from_millis(200),
            || false,
            || Ok(false),
        )
        .unwrap_err();
        assert!(err.is::<process::Canceled>(), "{err:#}");
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
