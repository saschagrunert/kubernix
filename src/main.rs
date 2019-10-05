use failure::Fallible;
use kubernix::{Config, Kubernix};
use std::process::exit;

pub fn main() {
    if let Err(e) = run() {
        println!("Error: {}", e);
        exit(1);
    }
}

fn run() -> Fallible<()> {
    // Parse CLI arguments
    let config = Config::default();

    if config.subcommand().is_some() {
        // Spawn only a new shell
        Kubernix::new_shell(config)
    } else {
        // Run kubernix
        Kubernix::start(config)
    }
}
