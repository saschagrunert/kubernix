mod common;

use common::{find_executable, run_local_test, run_root, SUDO};
use failure::{bail, Fallible};
use std::{net::Ipv4Addr, process::Command};

#[test]
fn local_single_node() -> Fallible<()> {
    let test = "e2e-single-node";
    run_local_test(test, None, || {
        let kubeconfig = run_root(test).join("kubeconfig").join("admin.kubeconfig");
        let e2e = find_executable("e2e.test")?;
        if !Command::new(SUDO)
            .arg("-E")
            .arg(e2e)
            .arg("--provider=local")
            .arg("--ginkgo.focus=.*\\[Conformance\\].*")
            .env("KUBECONFIG", kubeconfig)
            .env("KUBERNETES_SERVICE_HOST", Ipv4Addr::LOCALHOST.to_string())
            .env("KUBERNETES_SERVICE_PORT", "6443")
            .status()?
            .success()
        {
            bail!("e2e tests failed");
        }
        Ok(())
    })
}
