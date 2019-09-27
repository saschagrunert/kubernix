use clap::{crate_version, load_yaml, App};
use failure::{format_err, Fallible};
use kubernix::{Config, Kubernix};
use log::info;
use std::{
    env::set_var,
    process::exit,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
};

pub fn main() {
    if let Err(e) = run() {
        println!("{}", e);
        exit(1);
    }
}

fn run() -> Fallible<()> {
    // Parse CLI arguments
    let yaml = load_yaml!("cli.yaml");
    let matches = App::from_yaml(yaml).version(crate_version!()).get_matches();

    // Load config file
    let config_filename = matches
        .value_of("config")
        .ok_or_else(|| format_err!("No 'config' provided"))?;
    let config = Config::from_file(config_filename)?;
    set_var("RUST_LOG", format!("kubernix={}", config.log.level));
    env_logger::init();

    // Setup signal handler
    let running = Arc::new(AtomicBool::new(true));
    let r = running.clone();
    ctrlc::set_handler(move || {
        r.store(false, Ordering::SeqCst);
    })?;

    // Run kubernix
    info!("Starting kubernix");
    let mut kube = Kubernix::new(&config)?;

    // Cleanup on sigint
    while running.load(Ordering::SeqCst) {}
    info!("Cleaning up");
    kube.stop();
    Ok(())
}
