use clap::{crate_version, load_yaml, App};
use failure::{format_err, Fallible};
use ipnetwork::IpNetwork;
use kubernix::{ConfigBuilder, Kubernix};
use log::LevelFilter;
use std::process::exit;

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
    if let Some(x) = matches.value_of("root") {
        config_builder.root(x);
    }
    if let Some(x) = matches.value_of("log-level") {
        config_builder.log_level(x.parse::<LevelFilter>()?);
    }
    if let Some(x) = matches.value_of("crio-cidr") {
        config_builder.crio_cidr(x.parse::<IpNetwork>()?);
    }
    if let Some(x) = matches.value_of("cluster-cidr") {
        config_builder.cluster_cidr(x.parse::<IpNetwork>()?);
    }
    if let Some(x) = matches.value_of("service-cidr") {
        config_builder.service_cidr(x.parse::<IpNetwork>()?);
    }
    if let Some(x) = matches.value_of("overlay") {
        config_builder.overlay(x);
    }
    if matches.is_present("impure") {
        config_builder.impure(true);
    }
    let config = config_builder
        .build()
        .map_err(|e| format_err!("Unable to build config: {}", e))?;

    if matches.subcommand_matches("shell").is_some() {
        // Spawn only a new shell
        Kubernix::new_shell(config)?;
    } else {
        // Run kubernix
        Kubernix::start(config)?;
    }

    Ok(())
}
