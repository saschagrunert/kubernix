use anyhow::Result;
use kubernix::{Config, EXIT_NOT_RUNNING, Kubernix, Logger, SubCommand};
use std::process::exit;

pub fn main() {
    match run() {
        Ok(code) => exit(code),
        Err(e) => {
            Logger::fatal(&e);
            exit(1);
        }
    }
}

/// Run kubernix and return the exit code.
fn run() -> Result<i32> {
    // Parse CLI arguments
    let config = Config::from_args();

    let running = match config.subcommand() {
        // Bootstrap the cluster
        None => Kubernix::start(config).map(|()| true)?,
        // Spawn only a new shell or run a command in it
        Some(SubCommand::Shell { .. }) => Kubernix::new_shell(config)?,
        Some(SubCommand::Stop { .. }) => Kubernix::stop_cluster(config)?,
        Some(SubCommand::Status { .. }) => Kubernix::status(config)?,
    };
    Ok(if running { 0 } else { EXIT_NOT_RUNNING })
}
