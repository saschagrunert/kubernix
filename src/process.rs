use crate::Config;
use failure::{bail, format_err, Fallible};
use log::{debug, error, warn};
use std::{
    fs::File,
    io::{BufRead, BufReader},
    path::PathBuf,
    process::{Command, Stdio},
    sync::mpsc::{channel, Sender},
    thread,
    time::Instant,
};

/// The maximum wait time for processes to become ready
const READYNESS_TIMEOUT: u64 = 30;

/// A general process abstraction
pub struct Process {
    command: String,
    kill: Sender<()>,
    log_file: PathBuf,
}

/// The trait to stop something
pub trait Stoppable {
    fn stop(&mut self);
}

impl Process {
    /// Creates a new `Process` instance by spawning the provided command `cmd`.
    /// If the process creation fails, an `Error` will be returned.
    pub fn new(config: &Config, command: &[String]) -> Fallible<Process> {
        // Prepare the commands
        let cmd = command
            .get(0)
            .map(String::to_owned)
            .ok_or_else(|| format_err!("No valid command provided"))?;
        let args: Vec<String> =
            command.iter().map(|x| x.to_owned()).skip(1).collect();

        let log_file = &config
            .root
            .join(config.log.dir.join(format!("{}.log", cmd)));
        let out_file = File::create(&log_file)?;
        let err_file = out_file.try_clone()?;

        // Spawn the process child
        let mut child = Command::new(cmd.clone())
            .args(&args)
            .stderr(Stdio::from(err_file))
            .stdout(Stdio::from(out_file))
            .spawn()?;

        let (kill_tx, kill_rx) = channel();
        let c = cmd.clone();
        thread::spawn(move || loop {
            // Verify that the process is still running
            match child.try_wait() {
                Ok(Some(s)) => {
                    error!("Process '{}' died: {}", c, s);
                    break;
                }
                Err(e) => error!("Unable to wait for process: {}", e),
                Ok(None) => {} // process still running
            }

            // Kill the process if requested
            if kill_rx.try_recv().is_ok() {
                debug!("Stopping process '{}'", c);
                if child.kill().is_err() {
                    error!("Unable to kill process '{}'", c)
                }
                break;
            }
        });

        Ok(Process {
            command: format!("{} {}", cmd, args.join(" ")),
            kill: kill_tx,
            log_file: log_file.clone(),
        })
    }

    // Wait for the process to become ready, by searching for the pattern in
    // every line of its output.
    pub fn wait_ready(&mut self, pattern: &str) -> Fallible<()> {
        debug!(
            "Waiting for process '{}' to become ready with pattern: '{}'",
            self.command, pattern
        );
        let now = Instant::now();
        let file = File::open(&self.log_file)?;
        let mut reader = BufReader::new(file);

        while now.elapsed().as_secs() < READYNESS_TIMEOUT {
            let mut line = String::new();
            reader.read_line(&mut line)?;

            if line.contains(pattern) {
                debug!("Found pattern '{}' in line '{}'", pattern, line);
                return Ok(());
            }
        }

        // Cleanup since process is not ready
        self.stop();
        bail!("Timed out waiting for process to become ready")
    }
}

impl Stoppable for Process {
    /// Stopping the process by killing it
    fn stop(&mut self) {
        if self.kill.send(()).is_err() {
            warn!("Unable to kill process '{}'", self.command);
        }
    }
}
