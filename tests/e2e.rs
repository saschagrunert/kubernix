mod common;

use common::{run_local_test, run_root, SUDO};
use failure::{bail, Fallible};
use std::{env::var, process::Command};

#[test]
fn local_single_node_conformance() -> Fallible<()> {
    e2e_local_single_node("NodeConformance")
}

fn e2e_local_single_node(focus: &str) -> Fallible<()> {
    let test = &format!("e2e-{}", focus);
    run_local_test(test, None, || {
        let kubeconfig = run_root(test).join("kubeconfig").join("admin.kubeconfig");
        if !Command::new(SUDO)
            .arg("env")
            .arg(format!("PATH={}", var("PATH")?))
            .arg(format!("KUBECONFIG={}", kubeconfig.display()))
            .arg("KUBERNETES_SERVICE_HOST=127.0.0.1")
            .arg("KUBERNETES_SERVICE_PORT=6443")
            .arg("e2e.test")
            .arg("--provider=local")
            .arg(format!("--ginkgo.focus=.*\\[{}\\].*", focus))
            .arg("--ginkgo.dryRun")
            .status()?
            .success()
        {
            bail!("e2e tests failed");
        }
        Ok(())
    })
}
