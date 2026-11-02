//! Generic process lifecycle management.
//!
//! Provides the [`Process`] abstraction that spawns a child process,
//! watches it for unexpected exits, waits for readiness based on log
//! output patterns, and handles graceful shutdown via SIGTERM.

use crate::{signals, system::System};
use anyhow::{Context, Result, bail};
use log::{debug, error, info};
use nix::{
    errno::Errno,
    mount::{MsFlags, mount},
    sched::{CloneFlags, unshare},
    sys::signal::{Signal, kill},
    unistd::Pid,
};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    ffi::{CStr, CString},
    fs::{self, File, create_dir_all},
    io::{BufRead, BufReader},
    os::unix::{ffi::OsStrExt, process::CommandExt},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{
        Mutex,
        mpsc::{Receiver, SyncSender, sync_channel},
    },
    thread::{JoinHandle, sleep, spawn},
    time::{Duration, Instant},
};

/// A general process abstraction
pub struct Process {
    command: String,
    died: Receiver<()>,
    kill: SyncSender<()>,
    log_file: PathBuf,
    name: String,
    pid: u32,
    readiness_timeout: u64,
    watch: Option<JoinHandle<Result<()>>>,
}

/// The trait to stop something
pub trait Stoppable {
    /// Stop the process
    fn stop(&mut self) -> Result<()>;

    /// Returns true if the process exited without being stopped
    fn died(&mut self) -> bool;
}

/// A running process handle that can be stopped during cluster shutdown.
pub type Started = Box<dyn Stoppable + Send>;

/// Ordered collection of running processes, stopped in reverse order during cleanup.
pub type Stoppables = Vec<Started>;

/// The result of starting a component process, either a stoppable handle or an error.
pub type ProcessState = Result<Started>;

/// Default timeout in seconds for process readiness and pod polling.
pub const READINESS_TIMEOUT: u64 = 120;

/// The process groups of all started processes which have not been
/// stopped yet, with the start time of their leader.
static GROUPS: Mutex<BTreeMap<i32, Option<u64>>> = Mutex::new(BTreeMap::new());

/// The process groups of all running processes, with the start time of
/// their leader.
pub fn running_groups() -> Vec<(i32, Option<u64>)> {
    GROUPS
        .lock()
        .map(|g| g.iter().map(|(pid, start)| (*pid, *start)).collect())
        .unwrap_or_default()
}

/// Kill the provided process groups immediately. A group is skipped if
/// the PID of its leader belongs to a process with another start time,
/// since the leader exited and its PID got reused. Without a leader, the
/// PID cannot be reused as long as the group has members.
pub fn kill_groups(groups: &[(i32, Option<u64>)]) {
    for (pid, start) in groups {
        if let (Some(start), Some(stat)) = (start, ProcStat::read(*pid))
            && stat.start_time != *start
        {
            debug!("Skipping process group {}, its PID got reused", pid);
            continue;
        }
        debug!("Killing process group {}", pid);
        let _ = kill(Pid::from_raw(-pid), Signal::SIGKILL);
    }
}

/// Kill the process groups of all running processes immediately, for
/// example to force quit during the cleanup.
pub fn kill_all() {
    kill_groups(&running_groups());
}

fn set_running(pid: i32, running: bool) {
    if let Ok(mut groups) = GROUPS.lock() {
        if running {
            groups.insert(pid, ProcStat::read(pid).map(|s| s.start_time));
        } else {
            groups.remove(&pid);
        }
    }
}

/// The fields of `/proc/<pid>/stat` used by kubernix.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ProcStat {
    /// The process state, for example `R`, `S` or `Z` for a zombie
    pub state: char,
    /// The PID of the parent process
    pub ppid: i32,
    /// The start time after boot in clock ticks
    pub start_time: u64,
}

impl ProcStat {
    /// Read the stat of the process, if it exists.
    pub fn read(pid: i32) -> Option<Self> {
        Self::parse(&fs::read_to_string(format!("/proc/{}/stat", pid)).ok()?)
    }

    /// Parse the content of `/proc/<pid>/stat`. The command name may
    /// contain spaces and parentheses, so the fields start after the last
    /// closing parenthesis.
    fn parse(content: &str) -> Option<Self> {
        let mut fields = content.get(content.rfind(')')? + 1..)?.split_whitespace();
        let state = fields.next()?.chars().next()?;
        let ppid = fields.next()?.parse().ok()?;
        // The start time is field 22, the parent PID field 4
        let start_time = fields.nth(17)?.parse().ok()?;
        Some(Self {
            state,
            ppid,
            start_time,
        })
    }

    /// Returns true if the process exists and is not a zombie.
    pub fn alive(pid: i32) -> bool {
        Self::read(pid).is_some_and(|s| s.state != 'Z')
    }

    /// Returns true if `ancestor` is a parent or grandparent (and so on) of
    /// the current process.
    pub fn is_ancestor(ancestor: i32) -> bool {
        let mut pid = i32::try_from(std::process::id()).unwrap_or_default();
        while pid > 1 {
            match Self::read(pid) {
                Some(stat) if stat.ppid == ancestor => return true,
                Some(stat) => pid = stat.ppid,
                None => return false,
            }
        }
        false
    }
}

macro_rules! stoppable {
    ($ty:ty) => {
        impl $crate::process::Stoppable for $ty {
            fn stop(&mut self) -> anyhow::Result<()> {
                self.process.stop()
            }

            fn died(&mut self) -> bool {
                self.process.died()
            }
        }
    };
}
pub(crate) use stoppable;

#[derive(Deserialize, Serialize)]
struct Run {
    command: PathBuf,
    args: Vec<String>,
}

impl Process {
    /// Creates a new `Process` instance by spawning the provided `command` and `args`.
    /// If the process creation fails, an `Error` will be returned.
    pub fn start(dir: &Path, identifier: &str, command: &str, args: &[&str]) -> Result<Process> {
        Self::spawn(dir, identifier, command, args, None)
    }

    /// Like [`Process::start`], but runs the process in a new mount namespace
    /// in which `source` is bind mounted over `target`.
    pub fn start_with_bind_mount(
        dir: &Path,
        identifier: &str,
        command: &str,
        args: &[&str],
        source: &Path,
        target: &Path,
    ) -> Result<Process> {
        let source = CString::new(source.as_os_str().as_bytes())
            .context("Invalid bind mount source path")?;
        let target = CString::new(target.as_os_str().as_bytes())
            .context("Invalid bind mount target path")?;
        Self::spawn(dir, identifier, command, args, Some((source, target)))
    }

    fn spawn(
        dir: &Path,
        identifier: &str,
        command: &str,
        args: &[&str],
        bind_mount: Option<(CString, CString)>,
    ) -> Result<Process> {
        // Prepare the commands
        if command.is_empty() {
            bail!("No valid command provided")
        }
        info!("Starting {}", identifier);

        // Write the executed command into the dir
        create_dir_all(dir).context("Unable to create process directory")?;

        let run_file = dir.join("run.json");
        let run = Run {
            command: System::find_executable(command)?,
            args: args.iter().map(|x| (*x).to_string()).collect(),
        };
        fs::write(&run_file, serde_json::to_string_pretty(&run)?)
            .context("Unable to write process run file")?;

        // Prepare the log dir and file
        let mut log_file = dir.join(command);
        log_file.set_extension("log");
        let out_file = File::create(&log_file).context("Unable to create process log file")?;
        let err_file = out_file
            .try_clone()
            .context("Unable to clone process log file handle")?;

        // Spawn the process child in its own session so we can kill the
        // entire process group during shutdown.
        let mut cmd = Command::new(run.command);
        cmd.args(run.args)
            .stderr(Stdio::from(err_file))
            .stdout(Stdio::from(out_file));
        // SAFETY: setsid(), unshare() and mount() are plain syscalls, and the
        // paths are prepared before forking, so nothing allocates.
        unsafe {
            cmd.pre_exec(move || {
                nix::unistd::setsid()?;
                if let Some((source, target)) = &bind_mount {
                    unshare(CloneFlags::CLONE_NEWNS)?;
                    mount(
                        Some(source.as_c_str()),
                        target.as_c_str(),
                        None::<&CStr>,
                        MsFlags::MS_BIND,
                        None::<&CStr>,
                    )?;
                }
                Ok(())
            });
        }
        let mut child = cmd
            .spawn()
            .with_context(|| format!("Unable to start process '{}' ({})", identifier, command))?;

        // Start the watcher thread
        let (kill, killed) = sync_channel(1);
        let (dead, died) = sync_channel(1);
        let c = command.to_owned();
        let n = identifier.to_owned();
        let pid = child.id();
        if let Ok(pid) = i32::try_from(pid) {
            set_running(pid, true);
        }
        let lf = log_file.clone();
        let watch = spawn(move || {
            // Wait for the process to exit
            let status = child.wait()?;

            // No kill send, we assume that the process died
            if killed.try_recv().is_err() {
                error!("{} ({}) died unexpectedly", n, c);
                Self::dump_log_tail(&lf, &n);
                dead.send(())?;
            } else {
                info!("{} stopped", n);
            }
            debug!("{} ({}) {}", n, c, status);
            Ok(())
        });

        Ok(Process {
            command: command.into(),
            died,
            kill,
            log_file,
            name: identifier.into(),
            pid,
            readiness_timeout: READINESS_TIMEOUT,
            watch: Some(watch),
        })
    }

    /// Wait for the process to become ready, by searching for the pattern in
    /// every line of its output.
    pub fn wait_ready(&mut self, pattern: &str) -> Result<()> {
        debug!(
            "Waiting for process '{}' ({}) to become ready with pattern: '{}'",
            self.name, self.command, pattern
        );
        let now = Instant::now();
        let file = File::open(&self.log_file)?;
        let mut reader = BufReader::new(file);
        let mut line = String::new();

        while now.elapsed().as_secs() < self.readiness_timeout {
            // A line without a trailing newline is incomplete, keep it and
            // append the rest of it on the next read.
            let read = reader.read_line(&mut line)?;
            if read > 0 && line.contains(pattern) {
                info!("{} is ready", self.name);
                debug!("Found pattern '{}' in line '{}'", pattern, line.trim());
                return Ok(());
            }
            if line.ends_with('\n') {
                line.clear();
                continue;
            }

            if self.died.try_recv().is_ok() {
                bail!(
                    "{} ({}) died before becoming ready",
                    self.name,
                    self.command
                )
            }
            if signals::requested() {
                bail!(
                    "Interrupted while waiting for {} ({}) to become ready",
                    self.name,
                    self.command
                )
            }
            sleep(Duration::from_millis(10));
        }

        // Dump log before stopping so the file is still intact
        Self::dump_log_tail(&self.log_file, &self.name);
        self.stop()?;
        bail!(
            "Timed out after {}s waiting for process '{}' ({}) to become ready \
             with pattern '{}' (log: {})",
            self.readiness_timeout,
            self.name,
            self.command,
            pattern,
            self.log_file.display(),
        )
    }

    /// Dump the last lines of a process log file for debugging
    fn dump_log_tail(log_file: &Path, name: &str) {
        if let Ok(content) = fs::read_to_string(log_file) {
            let lines: Vec<&str> = content.lines().collect();
            let start = lines.len().saturating_sub(20);
            error!("Last log lines of {}:", name);
            for line in &lines[start..] {
                error!("  {}", line);
            }
        }
    }
}

impl Stoppable for Process {
    /// Stopping the process by killing it
    fn stop(&mut self) -> Result<()> {
        debug!("Stopping process {} (via {})", self.name, self.command);

        // Indicate that this shutdown is intended. This fails if the
        // process already died and the watcher is gone, which is fine since
        // its descendants still have to be stopped.
        let _ = self.kill.try_send(());

        let pid = i32::try_from(self.pid)
            .with_context(|| format!("PID {} exceeds i32 range", self.pid))?;
        let pgid = Pid::from_raw(-pid);

        match kill(pgid, Signal::SIGTERM) {
            Ok(()) | Err(Errno::ESRCH) => {}
            Err(e) => {
                return Err(e).with_context(|| {
                    format!(
                        "Unable to send SIGTERM to process group of {} (via {})",
                        self.name, self.command,
                    )
                });
            }
        }

        if let Some(handle) = self.watch.take() {
            let deadline = Instant::now() + Duration::from_secs(10);
            while !handle.is_finished() && Instant::now() < deadline {
                sleep(Duration::from_millis(100));
            }
            if !handle.is_finished() {
                debug!(
                    "Process {} (via {}) did not exit after SIGTERM, sending SIGKILL",
                    self.name, self.command,
                );
                let _ = kill(pgid, Signal::SIGKILL);
            }
            let joined = handle.join();
            set_running(pid, false);
            if joined.is_err() {
                bail!(
                    "Unable to stop process {} (via {})",
                    self.name,
                    self.command
                );
            }
        }
        debug!("Process {} (via {}) stopped", self.name, self.command);
        Ok(())
    }

    fn died(&mut self) -> bool {
        self.watch.is_some() && self.died.try_recv().is_ok()
    }
}

impl Drop for Process {
    fn drop(&mut self) {
        if self.watch.is_some()
            && let Err(e) = self.stop()
        {
            debug!("Failed to stop process {} during drop: {}", self.name, e);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn start_with_bind_mount_invalid_path() {
        let dir = tempdir().unwrap();
        let invalid = Path::new("/invalid\0path");
        assert!(
            Process::start_with_bind_mount(
                dir.path(),
                "test",
                "true",
                &[],
                invalid,
                Path::new("/")
            )
            .is_err()
        );
        assert!(
            Process::start_with_bind_mount(
                dir.path(),
                "test",
                "true",
                &[],
                Path::new("/"),
                invalid
            )
            .is_err()
        );
    }

    #[test]
    fn start_success() -> Result<()> {
        let d = tempdir()?;
        Process::start(d.path(), "", "echo", &[])?;
        Ok(())
    }

    #[test]
    fn start_failure_no_command() -> Result<()> {
        let d = tempdir()?;
        assert!(Process::start(d.path(), "", "", &[]).is_err());
        Ok(())
    }

    #[test]
    fn start_failure_invalid_command() -> Result<()> {
        let d = tempdir()?;
        assert!(Process::start(d.path(), "", "invalid_command", &[]).is_err());
        Ok(())
    }

    #[test]
    fn wait_ready_success() -> Result<()> {
        let d = tempdir()?;
        let mut p = Process::start(d.path(), "", "echo", &["test"])?;
        p.wait_ready("test")?;
        Ok(())
    }

    #[test]
    fn wait_ready_failure() -> Result<()> {
        let d = tempdir()?;
        let mut p = Process::start(d.path(), "", "echo", &["test"])?;
        p.readiness_timeout = 1;
        assert!(p.wait_ready("invalid").is_err());
        Ok(())
    }

    #[test]
    fn wait_ready_partial_line() -> Result<()> {
        let d = tempdir()?;
        let mut p = Process::start(
            d.path(),
            "",
            "sh",
            &["-c", "printf rea; sleep 0.5; printf 'dy\\n'; sleep 5"],
        )?;
        p.readiness_timeout = 3;
        p.wait_ready("ready")?;
        p.stop()?;
        Ok(())
    }

    #[test]
    fn stop_success() -> Result<()> {
        let d = tempdir()?;
        let mut p = Process::start(d.path(), "", "sleep", &["500"])?;
        assert!(!p.died());
        p.stop()?;
        Ok(())
    }

    #[test]
    fn running_groups_tracks_processes() -> Result<()> {
        let tracked = |pid| running_groups().iter().any(|(p, _)| *p == pid);
        let d = tempdir()?;
        let mut p = Process::start(d.path(), "", "sleep", &["500"])?;
        let pid = i32::try_from(p.pid)?;
        let start = ProcStat::read(pid).context("no stat")?.start_time;
        assert!(running_groups().contains(&(pid, Some(start))));

        // A different start time means that the PID got reused
        kill_groups(&[(pid, Some(start + 1))]);
        sleep(Duration::from_millis(200));
        assert!(!p.died());

        kill_groups(&[(pid, Some(start))]);
        let deadline = Instant::now() + Duration::from_secs(5);
        while !p.died() {
            assert!(Instant::now() < deadline, "process group not killed");
            sleep(Duration::from_millis(10));
        }
        assert!(tracked(pid));

        p.stop()?;
        assert!(!tracked(pid));
        Ok(())
    }

    #[test]
    fn proc_stat_parse() {
        let stat = "42 (a) b (c)) S 1 42 42 0 -1 4194560 100 0 0 0 0 0 0 0 20 0 1 0 12345 \
                    8192 100 18446744073709551615";
        assert_eq!(
            ProcStat::parse(stat),
            Some(ProcStat {
                state: 'S',
                ppid: 1,
                start_time: 12345
            })
        );
        assert_eq!(ProcStat::parse("42 (a) Z"), None);
        assert_eq!(ProcStat::parse("garbage"), None);
    }

    #[test]
    fn proc_stat_alive() {
        assert!(ProcStat::alive(
            i32::try_from(std::process::id()).unwrap_or_default()
        ));
        assert!(!ProcStat::alive(i32::MAX));
    }

    #[test]
    fn proc_stat_is_ancestor() -> Result<()> {
        let pid = i32::try_from(std::process::id())?;
        let parent = ProcStat::read(pid).context("no stat")?.ppid;
        assert!(ProcStat::is_ancestor(parent));
        assert!(!ProcStat::is_ancestor(pid));
        assert!(!ProcStat::is_ancestor(i32::MAX));
        Ok(())
    }

    #[test]
    fn stop_after_death() -> Result<()> {
        let d = tempdir()?;
        let mut p = Process::start(d.path(), "", "true", &[])?;
        let deadline = Instant::now() + Duration::from_secs(5);
        while !p.died() && Instant::now() < deadline {
            sleep(Duration::from_millis(10));
        }
        p.stop()?;
        assert!(p.watch.is_none());
        Ok(())
    }
}
