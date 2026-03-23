mod common;

use anyhow::Result;
use common::{none_hook, run_local_test};

#[test]
fn local_single_node() -> Result<()> {
    run_local_test("local-single", None, none_hook)
}

#[test]
fn local_multi_node() -> Result<()> {
    run_local_test("local-multi", Some(&["--nodes=2"]), none_hook)
}
