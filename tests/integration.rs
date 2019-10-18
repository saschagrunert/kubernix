mod common;

use common::{none_hook, run_docker_test, run_local_test, run_podman_test};
use failure::Fallible;

#[test]
fn local_single_node() -> Fallible<()> {
    run_local_test("integration-local-single", None, none_hook)
}

#[test]
fn local_multi_node() -> Fallible<()> {
    run_local_test("integration-local-multi", Some(&["--nodes=2"]), none_hook)
}

#[test]
fn docker_single_node() -> Fallible<()> {
    run_docker_test("integration-docker-single", None)
}

#[test]
fn podman_single_node() -> Fallible<()> {
    run_podman_test("integration-podman-single", None)
}

#[test]
fn podman_multi_node() -> Fallible<()> {
    run_podman_test("integration-podman-multi", Some(&["--nodes=2"]))
}
