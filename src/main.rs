use clap::{crate_version, load_yaml, App};
use failure::{format_err, Fallible};
use kubernix::{ConfigBuilder, Kubernix};
use std::{env::set_var, process::exit};

pub fn main() {
    if let Err(e) = run() {
        println!("Error: {}", e);
        exit(1);
    }
}

fn run() -> Fallible<()> {
    // Parse CLI arguments
    let yaml = load_yaml!("cli.yaml");
    let matches = App::from_yaml(yaml).version(crate_version!()).get_matches();

    // Build the config
    let mut config_builder = ConfigBuilder::default();
    if matches.is_present("verbose") {
        config_builder.log_level("debug");
    }
    if let Some(x) = matches.value_of("root") {
        config_builder.root(x);
    }
    let mut config = config_builder
        .build()
        .map_err(|e| format_err!("Unable to build config: {}", e))?;
    config.prepare()?;

    // Setup the logger
    set_var("RUST_LOG", format!("kubernix={}", config.log_level()));
    env_logger::init();

    // Run kubernix
    Kubernix::start(config)?;

    Ok(())
}
