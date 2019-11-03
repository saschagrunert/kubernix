use anyhow::Result;
use kubernix::{Config, Kubernix, Logger};
use std::process::exit;

pub fn main() {
    if let Err(e) = run() {
        Logger::error(
            &e.chain()
                .map(|x| x.to_string())
                .collect::<Vec<_>>()
                .join(": "),
        );
        exit(1);
    }
}

fn run() -> Result<()> {
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
