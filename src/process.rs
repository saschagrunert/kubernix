use crate::Config;
use failure::{bail, format_err, Fallible};
use log::debug;
use nix::sys::signal::{kill, Signal};
use nix::unistd::Pid;
use std::{
    fs::File,
    io::{BufRead, BufReader},
    path::PathBuf,
    process::{Command, Stdio},
    sync::mpsc::{channel, Sender},
    thread::{spawn, JoinHandle},
    time::Instant,
};

/// The maximum wait time for processes to become ready
const READYNESS_TIMEOUT: u64 = 30;

/// A general process abstraction
pub struct Process {
    command: String,
    kill: Sender<()>,
    log_file: PathBuf,
    watch: Option<JoinHandle<Fallible<()>>>,
}

/// The trait to stop something
pub trait Stoppable {
    /// Stop the process
    fn stop(&mut self) -> Fallible<()>;
}

/// Starable process type
pub type Startable = Box<dyn Stoppable + Send>;

impl Process {
    /// Creates a new `Process` instance by spawning the provided command `cmd`.
    /// If the process creation fails, an `Error` will be returned.
    pub fn start(config: &Config, command: &[String]) -> Fallible<Process> {
        // Prepare the commands
        let cmd = command
            .get(0)
            .map(String::to_owned)
            .ok_or_else(|| format_err!("No valid command provided"))?;
        let args: Vec<String> = command.iter().map(|x| x.to_owned()).skip(1).collect();

        let mut log_file = config.root.join(&config.log.dir).join(&cmd);
        log_file.set_extension("log");

        let out_file = File::create(&log_file)?;
        let err_file = out_file.try_clone()?;

        // Spawn the process child
        let mut child = Command::new(&cmd)
            .args(&args)
            .stderr(Stdio::from(err_file))
            .stdout(Stdio::from(out_file))
            .spawn()?;

        let (kill_tx, kill_rx) = channel();
        let c = cmd.clone();
        let watch = spawn(move || {
            loop {
                // Verify that the process is still running
                if let Some(s) = child.try_wait()? {
                    bail!("Process '{}' died unexpectedly: {}", c, s);
                }

                // Kill the process if requested
                if kill_rx.try_recv().is_ok() {
                    debug!("Stopping process '{}'", c);
                    match kill(Pid::from_raw(child.id() as i32), Signal::SIGTERM) {
                        Ok(_) => {
                            debug!("Waiting for '{}' to exit", c);
                            child.wait()?;
                        }
                        Err(e) => {
                            bail!("Unable to kill process '{}': {}", c, e);
                        }
                    }
                }
            }
        });

        Ok(Process {
            command: cmd,
            kill: kill_tx,
            log_file,
            watch: Some(watch),
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
        self.kill.send(())?;
        bail!("Timed out waiting for process to become ready")
    }
}

impl Stoppable for Process {
    /// Stopping the process by killing it
    fn stop(&mut self) -> Fallible<()> {
        self.kill.send(())?;
        if let Some(handle) = self.watch.take() {
            if handle.join().is_err() {
                bail!("Unable to stop process '{}'", self.command);
            }
        }
        debug!("Process '{}' stopped", self.command);
        Ok(())
    }
}
