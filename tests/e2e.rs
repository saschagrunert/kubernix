mod common;

use common::{run_local_test, run_root, SUDO};
use failure::{bail, Fallible};
use std::{net::Ipv4Addr, process::Command};

#[test]
fn local_single_node() -> Fallible<()> {
    let test = "e2e-single-node";
    run_local_test(test, None, || {
        let kubeconfig = run_root(test).join("kubeconfig").join("admin.kubeconfig");
        if !Command::new("e2e.test")
            .arg("--provider=local")
            // TODO: enable more tests
            // .arg("--ginkgo.focus=.*\\[Conformance\\].*")
            .arg("--ginkgo.focus=.*should serve a basic endpoint from pods.*")
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
