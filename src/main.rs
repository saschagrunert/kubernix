use anyhow::Result;
use console::style;
use kubernix::{Config, Kubernix};
use std::process::exit;

pub fn main() {
    if let Err(e) = run() {
        println!(
            "{}{}{} {}",
            style("[").white().dim(),
            style("ERROR").red(),
            style("]").white().dim(),
            e.chain()
                .map(|x| x.to_string())
                .collect::<Vec<_>>()
                .join(": ")
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
