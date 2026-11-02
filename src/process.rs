//! Generic process lifecycle management.
//!
//! Provides the [`Process`] abstraction that spawns a child process,
//! watches it for unexpected exits, waits for readiness based on log
//! output patterns, and handles graceful shutdown via SIGTERM.
//!
//! All process groups which kubernix starts are tracked, so that a force
//! quit can kill them. The ones of the components are saved in the run
//! root as well, so that the next start can kill the leftovers of an
//! unclean exit.

use crate::{
    files::{create_file, write_unsynced},
    signals,
    system::System,
    write_file,
};
use anyhow::{Context, Result, bail};
use log::{debug, error, info, warn};
use nix::{
    errno::Errno,
    mount::{MsFlags, mount},
    sched::{CloneFlags, unshare},
    sys::signal::{Signal, kill},
    unistd::Pid,
};
use serde::{Deserialize, Serialize};
use std::{
    cell::RefCell,
    collections::BTreeMap,
    ffi::{CStr, CString, OsStr},
    fmt,
    fs::{self, File, create_dir_all},
    io::{self, BufRead, BufReader, ErrorKind, Write},
    os::unix::{ffi::OsStrExt, process::CommandExt},
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle, sleep, spawn},
    time::{Duration, Instant},
};

/// A general process abstraction
pub struct Process {
    command: String,
    flags: Arc<Flags>,
    log_file: PathBuf,
    name: String,
    pid: i32,
    start_time: Option<u64>,
    readiness_timeout: u64,
    stop_timeout: Duration,
    watch: Option<JoinHandle<()>>,
}

/// The state shared between a process and its watcher thread.
#[derive(Default)]
struct Flags {
    /// Set once the process gets stopped intentionally
    stopping: AtomicBool,
    /// Set if the process exited without being stopped
    died: AtomicBool,
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

/// The time a process gets to exit after SIGTERM before it gets killed.
const STOP_TIMEOUT: Duration = Duration::from_secs(10);

/// The time a process group gets to vanish after SIGKILL.
const KILL_TIMEOUT: Duration = Duration::from_secs(1);

/// The interval for polling the exit of processes.
const POLL_INTERVAL: Duration = Duration::from_millis(10);

/// A tracked process group.
#[derive(Clone, Copy)]
struct Group {
    /// The start time of the group leader
    start: Option<u64>,
    /// Whether the group gets saved in the run root
    persist: bool,
}

/// The process groups of all started processes which have not been
/// stopped yet, and the file in the run root to save them in.
struct Groups {
    groups: BTreeMap<i32, Group>,
    file: Option<PathBuf>,
    /// Counts the changes of the persistent groups and the file
    generation: u64,
}

static GROUPS: Mutex<Groups> = Mutex::new(Groups {
    groups: BTreeMap::new(),
    file: None,
    generation: 0,
});

/// The generation of the groups which got saved last. Serializes the
/// writes of the groups file, while `GROUPS` stays available.
static SAVED_GENERATION: Mutex<u64> = Mutex::new(0);

/// The content of the process groups file in the run root.
#[derive(Debug, Deserialize, PartialEq, Eq, Serialize)]
struct SavedGroups {
    /// The groups are only valid during the boot they got saved in
    boot_id: String,
    /// The process groups with the start time of their leader
    groups: Vec<(i32, Option<u64>)>,
}

/// The persistent process groups at one point in time, to be saved
/// without holding the lock of `GROUPS`.
#[derive(Debug, PartialEq, Eq)]
struct Snapshot {
    file: PathBuf,
    generation: u64,
    groups: Vec<(i32, Option<u64>)>,
}

impl Groups {
    /// The persistent process groups, if they get saved.
    fn snapshot(&self) -> Option<Snapshot> {
        Some(Snapshot {
            file: self.file.clone()?,
            generation: self.generation,
            groups: self
                .groups
                .iter()
                .filter(|(_, g)| g.persist)
                .map(|(pid, g)| (*pid, g.start))
                .collect(),
        })
    }
}

impl Snapshot {
    /// Save the process groups, or remove the file if there are none. The
    /// file is best effort, so it does not get synced to disk, which would
    /// slow down the parallel start and stop of the components.
    fn save(&self) {
        let result = if self.groups.is_empty() {
            match fs::remove_file(&self.file) {
                Err(e) if e.kind() != ErrorKind::NotFound => Err(e.into()),
                _ => Ok(()),
            }
        } else {
            let saved = SavedGroups {
                boot_id: boot_id().unwrap_or_default(),
                groups: self.groups.clone(),
            };
            serde_json::to_vec(&saved)
                .map_err(Into::into)
                .and_then(|content| write_unsynced(&self.file, &content))
        };
        if let Err(e) = result {
            debug!("Unable to save process groups: {:#}", e);
        }
    }
}

/// Save the latest snapshot of the persistent process groups, unless it
/// got saved already. Writers which wait for each other take the latest
/// snapshot once it is their turn, so the last write is always the latest
/// state, and outdated ones get skipped.
fn save_groups() {
    let Ok(mut saved) = SAVED_GENERATION.lock() else {
        return;
    };
    let Some(snapshot) = GROUPS.lock().ok().and_then(|g| g.snapshot()) else {
        return;
    };
    if snapshot.generation != *saved {
        snapshot.save();
        *saved = snapshot.generation;
    }
}

/// The ID of the current boot.
fn boot_id() -> Option<String> {
    fs::read_to_string("/proc/sys/kernel/random/boot_id")
        .ok()
        .map(|id| id.trim().into())
}

/// Track the process group of the leader `pid`, which started at `start`.
fn track(pid: i32, start: Option<u64>, persist: bool) {
    if let Ok(mut groups) = GROUPS.lock() {
        groups.groups.insert(pid, Group { start, persist });
        if persist {
            groups.generation += 1;
        }
    }
    if persist {
        save_groups();
    }
}

fn untrack(pid: i32) {
    let changed = GROUPS.lock().is_ok_and(|mut groups| {
        let persist = groups.groups.remove(&pid).is_some_and(|g| g.persist);
        if persist {
            groups.generation += 1;
        }
        persist
    });
    if changed {
        save_groups();
    }
}

/// Kill the process groups which a previous kubernix instance saved in
/// `file` and left behind, and wait until they are gone, so that their
/// ports are free again. Save the process groups of the components there
/// from now on.
pub fn track_in(file: &Path) {
    kill_leftovers(file);
    if let Ok(mut groups) = GROUPS.lock() {
        groups.file = Some(file.into());
        groups.generation += 1;
    }
    save_groups();
}

/// Kill the process groups saved in `file`, if they belong to the current
/// boot.
fn kill_leftovers(file: &Path) {
    let content = match fs::read(file) {
        Ok(content) => content,
        Err(e) if e.kind() == ErrorKind::NotFound => return,
        Err(e) => {
            debug!("Unable to read '{}': {}", file.display(), e);
            return;
        }
    };
    match serde_json::from_slice::<SavedGroups>(&content) {
        Ok(saved) if Some(&saved.boot_id) == boot_id().as_ref() => {
            info!("Killing the leftover processes of a previous run");
            let killed = kill_groups(&saved.groups);
            let deadline = Instant::now() + KILL_TIMEOUT;
            for (pid, start) in killed {
                if !wait_group_exit(pid, start, deadline) {
                    warn!("Leftover process group {} did not exit after SIGKILL", pid);
                }
            }
        }
        Ok(_) => debug!("Ignoring process groups of a previous boot"),
        Err(e) => debug!("Invalid process groups file '{}': {}", file.display(), e),
    }
}

/// The process groups of all running processes, with the start time of
/// their leader.
fn running_groups() -> Vec<(i32, Option<u64>)> {
    GROUPS
        .lock()
        .map(|g| g.groups.iter().map(|(pid, g)| (*pid, g.start)).collect())
        .unwrap_or_default()
}

/// Returns true if the PID of the group leader belongs to a process with
/// another start time, since the leader exited and its PID got reused.
/// Without a leader, the PID cannot be reused as long as the group has
/// members.
fn reused(pid: i32, start: Option<u64>) -> bool {
    matches!(
        (start, ProcStat::read(pid)),
        (Some(start), Some(stat)) if stat.start_time != start
    )
}

/// Wait until `deadline` for the process group of the leader `pid` to have
/// no members anymore, returns true if it has none. A reused leader PID
/// means that the group is gone, since it cannot be reused before.
fn wait_group_exit(pid: i32, start: Option<u64>, deadline: Instant) -> bool {
    loop {
        if reused(pid, start) || kill(Pid::from_raw(-pid), None) == Err(Errno::ESRCH) {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        sleep(POLL_INTERVAL);
    }
}

/// Kill the provided process groups immediately, except the ones whose
/// leader PID got reused. Returns the killed ones, without waiting for
/// them to exit.
fn kill_groups(groups: &[(i32, Option<u64>)]) -> Vec<(i32, Option<u64>)> {
    let mut killed = vec![];
    for (pid, start) in groups {
        if reused(*pid, *start) {
            debug!("Skipping process group {}, its PID got reused", pid);
            continue;
        }
        debug!("Killing process group {}", pid);
        if kill(Pid::from_raw(-pid), Signal::SIGKILL).is_ok() {
            killed.push((*pid, *start));
        }
    }
    killed
}

/// Kill the process groups of all running processes immediately, for
/// example to force quit.
pub fn kill_all() {
    kill_groups(&running_groups());
}

/// A helper command, which runs in its own process group without terminal
/// input. So the signals of the terminal like Ctrl+C do not interrupt it,
/// for example during the cleanup.
pub fn command<S: AsRef<OsStr>>(program: S) -> Command {
    let mut cmd = Command::new(program);
    cmd.process_group(0).stdin(Stdio::null());
    cmd
}

/// Run the command in its own process group, write `input` to it and
/// collect its output, but terminate it as soon as `cancel` returns true.
/// A force quit kills it as well.
pub fn run_cancelable(
    cmd: &mut Command,
    input: Option<&[u8]>,
    cancel: impl Fn() -> bool + Sync,
) -> Result<Output> {
    cmd.process_group(0).stdin(if input.is_some() {
        Stdio::piped()
    } else {
        Stdio::null()
    });
    let mut child = cmd.spawn()?;
    let pid = i32::try_from(child.id())?;
    let start = ProcStat::read(pid).map(|s| s.start_time);
    track(pid, start, false);

    // Write the input in parallel, since the command may fill the output
    // pipes before reading all of it.
    let stdin = child.stdin.take();
    let done = AtomicBool::new(false);
    let (written, output, canceled) = thread::scope(|s| {
        let writer = s.spawn(move || match (stdin, input) {
            (Some(mut pipe), Some(data)) => pipe.write_all(data),
            _ => Ok(()),
        });
        let canceler = s.spawn(|| terminate_on_cancel(pid, start, &done, &cancel));
        let output = child.wait_with_output();
        done.store(true, Ordering::SeqCst);
        let written = writer
            .join()
            .unwrap_or_else(|_| Err(io::Error::other("writer thread panicked")));
        (written, output, canceler.join().unwrap_or_default())
    });
    untrack(pid);
    if canceled {
        bail!("Canceled")
    }
    let output = output?;
    // A failed command may not read all of its input
    if output.status.success() {
        written.context("Unable to write the command input")?;
    }
    Ok(output)
}

/// Terminate the process group once `cancel` returns true, until `done` is
/// set. Returns true if it got canceled.
fn terminate_on_cancel(
    pid: i32,
    start: Option<u64>,
    done: &AtomicBool,
    cancel: &impl Fn() -> bool,
) -> bool {
    let group = Pid::from_raw(-pid);
    while !done.load(Ordering::SeqCst) {
        if cancel() {
            if !reused(pid, start) {
                let _ = kill(group, Signal::SIGTERM);
            }
            let deadline = Instant::now() + Duration::from_secs(5);
            while !done.load(Ordering::SeqCst) {
                if Instant::now() >= deadline {
                    if !reused(pid, start) {
                        let _ = kill(group, Signal::SIGKILL);
                    }
                    break;
                }
                sleep(POLL_INTERVAL);
            }
            return true;
        }
        sleep(POLL_INTERVAL);
    }
    false
}

thread_local! {
    /// The flag which cancels the start of the component on this thread.
    static CANCEL: RefCell<Option<Arc<AtomicBool>>> = const { RefCell::new(None) };
}

/// Set the flag which cancels the start of the component on this thread,
/// for example once another component of the boot phase failed.
pub fn set_cancel(flag: Arc<AtomicBool>) {
    CANCEL.with(|c| *c.borrow_mut() = Some(flag));
}

/// The flag which cancels the start of the component on this thread, to
/// check it on other threads.
pub fn cancel_flag() -> Option<Arc<AtomicBool>> {
    CANCEL.with(|c| c.borrow().clone())
}

/// Returns true if the start of the component on this thread got canceled.
pub fn canceled() -> bool {
    CANCEL.with(|c| {
        c.borrow()
            .as_ref()
            .is_some_and(|f| f.load(Ordering::SeqCst))
    })
}

/// The error of a component which stopped waiting for its readiness,
/// since another component of the boot phase failed.
#[derive(Debug)]
pub struct Canceled;

impl fmt::Display for Canceled {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "another component failed")
    }
}

impl std::error::Error for Canceled {}

/// The PIDs and start times of the processes whose command line, with the
/// arguments separated by NUL bytes, satisfies the predicate.
fn scan_processes(predicate: impl Fn(&[u8]) -> bool) -> Vec<(i32, u64)> {
    let Ok(entries) = fs::read_dir("/proc") else {
        return vec![];
    };
    entries
        .flatten()
        .filter_map(|e| e.file_name().to_str()?.parse::<i32>().ok())
        .filter(|pid| fs::read(format!("/proc/{}/cmdline", pid)).is_ok_and(|c| predicate(&c)))
        .filter_map(|pid| Some((pid, ProcStat::read(pid)?.start_time)))
        .collect()
}

/// The PIDs and start times of the processes whose command line starts
/// with `executable` and contains `arg`.
pub fn find_processes(executable: &Path, arg: &str) -> Vec<(i32, u64)> {
    scan_processes(|c| cmdline_matches(c, executable, arg))
}

/// The PIDs and start times of the processes whose executable has one of
/// the file `names` and whose command line contains the path `dir` or a
/// path below it in any argument.
pub fn find_processes_containing(names: &[&str], dir: &str) -> Vec<(i32, u64)> {
    scan_processes(|c| cmdline_contains(c, names, dir))
}

/// Returns true if the executable of the command line has one of the file
/// `names` and an argument contains the path `dir` or a path below it.
/// The path has to start at the beginning of the argument or after a
/// separator like `=`, and has to end there or continue with `/`, so that
/// `/run/node-1` matches neither `/run/node-10` nor `/other/run/node-1`.
fn cmdline_contains(cmdline: &[u8], names: &[&str], dir: &str) -> bool {
    let dir = dir.trim_end_matches('/').as_bytes();
    if dir.is_empty() {
        return false;
    }
    let mut args = cmdline.split(|b| *b == 0);
    let executable = args.next().map(|a| Path::new(OsStr::from_bytes(a)));
    let Some(name) = executable.and_then(Path::file_name) else {
        return false;
    };
    if !names.iter().any(|n| OsStr::new(n) == name) {
        return false;
    }
    let separator = |b: u8| matches!(b, b'=' | b':' | b',') || b.is_ascii_whitespace();
    args.any(|arg| {
        arg.windows(dir.len()).enumerate().any(|(i, w)| {
            w == dir
                && (i == 0 || separator(arg[i - 1]))
                && arg
                    .get(i + dir.len())
                    .is_none_or(|b| *b == b'/' || separator(*b))
        })
    })
}

fn cmdline_matches(cmdline: &[u8], executable: &Path, arg: &str) -> bool {
    let mut args = cmdline.split(|b| *b == 0);
    args.next() == Some(executable.as_os_str().as_bytes()) && args.any(|a| a == arg.as_bytes())
}

/// Send the signal to the processes, unless their PID got reused.
pub fn signal_processes(processes: &[(i32, u64)], signal: Signal) {
    for (pid, start) in processes {
        if ProcStat::read(*pid).is_some_and(|s| s.start_time == *start) {
            let _ = kill(Pid::from_raw(*pid), signal);
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

    /// Returns true if the process exists, is not a zombie and started at
    /// `start`. Another start time means that the process exited and its
    /// PID got reused.
    pub fn alive_since(pid: i32, start: u64) -> bool {
        Self::read(pid).is_some_and(|s| s.state != 'Z' && s.start_time == start)
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
        Self::spawn(dir, identifier, command, args, &[], None)
    }

    /// Like [`Process::start`], but sets the environment variables `env`
    /// for the process.
    pub fn start_with_env(
        dir: &Path,
        identifier: &str,
        command: &str,
        args: &[&str],
        env: &[(&str, &str)],
    ) -> Result<Process> {
        Self::spawn(dir, identifier, command, args, env, None)
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
        Self::spawn(dir, identifier, command, args, &[], Some((source, target)))
    }

    fn spawn(
        dir: &Path,
        identifier: &str,
        command: &str,
        args: &[&str],
        env: &[(&str, &str)],
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
        write_file(&run_file, serde_json::to_string_pretty(&run)?.as_bytes())
            .context("Unable to write process run file")?;

        // Prepare the log dir and file
        let mut log_file = dir.join(command);
        log_file.set_extension("log");
        let out_file = create_file(&log_file).context("Unable to create process log file")?;
        let err_file = out_file
            .try_clone()
            .context("Unable to clone process log file handle")?;

        // Spawn the process child in its own session so we can kill the
        // entire process group during shutdown. It must not read from the
        // terminal.
        let mut cmd = Command::new(run.command);
        cmd.args(run.args)
            .envs(env.iter().copied())
            .stdin(Stdio::null())
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
        let Ok(pid) = i32::try_from(child.id()) else {
            let _ = child.kill();
            let _ = child.wait();
            bail!("PID of process '{}' exceeds i32 range", identifier)
        };
        // The child is not reaped before the watcher waits for it, so its
        // stat is the one of the started process
        let start_time = ProcStat::read(pid).map(|s| s.start_time);
        track(pid, start_time, true);

        // Start the watcher thread
        let flags = Arc::new(Flags::default());
        let watch_flags = Arc::clone(&flags);
        let c = command.to_owned();
        let n = identifier.to_owned();
        let lf = log_file.clone();
        let watch = spawn(move || {
            // Wait for the process to exit
            let status = child.wait();

            if watch_flags.stopping.load(Ordering::SeqCst) {
                info!("{} stopped", n);
            } else {
                error!("{} ({}) died unexpectedly", n, c);
                Self::dump_log_tail(&lf, &n);
                watch_flags.died.store(true, Ordering::SeqCst);
            }
            match status {
                Ok(status) => debug!("{} ({}) {}", n, c, status),
                Err(e) => debug!("Unable to wait for {} ({}): {}", n, c, e),
            }
        });

        Ok(Process {
            command: command.into(),
            flags,
            log_file,
            name: identifier.into(),
            pid,
            start_time,
            readiness_timeout: READINESS_TIMEOUT,
            stop_timeout: STOP_TIMEOUT,
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

            if self.flags.died.load(Ordering::SeqCst) {
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
            if canceled() {
                return Err(Canceled).with_context(|| {
                    format!(
                        "Stopped waiting for {} ({}) to become ready",
                        self.name, self.command
                    )
                });
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

    /// Wait up to `timeout` for the process to exit, returns true if it
    /// did.
    fn wait_exit(&self, timeout: Duration) -> bool {
        let Some(handle) = &self.watch else {
            return true;
        };
        let deadline = Instant::now() + timeout;
        while !handle.is_finished() {
            if Instant::now() >= deadline {
                return false;
            }
            sleep(POLL_INTERVAL);
        }
        true
    }

    /// Stop the processes which this process runs inside a container via
    /// the `exec` of the container runtime, since the runtime does not
    /// forward signals to them. They are found by their `executable` and an
    /// argument `arg` of their command line. The runtime exits once they
    /// exited, afterwards [`Stoppable::stop`] cleans it up.
    pub fn stop_exec(&mut self, executable: &Path, arg: &str) {
        if self.wait_exit(Duration::ZERO) {
            return;
        }
        self.flags.stopping.store(true, Ordering::SeqCst);
        let processes = find_processes(executable, arg);
        if processes.is_empty() {
            debug!("No processes of {} found in the container", self.name);
            return;
        }
        debug!("Stopping {} in the container {:?}", self.name, processes);
        signal_processes(&processes, Signal::SIGTERM);
        if !self.wait_exit(self.stop_timeout) {
            debug!("{} did not exit after SIGTERM, sending SIGKILL", self.name);
            signal_processes(&processes, Signal::SIGKILL);
            self.wait_exit(Duration::from_secs(1));
        }
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

        // Indicate that this shutdown is intended
        self.flags.stopping.store(true, Ordering::SeqCst);

        // The leader may have exited already, then its PID could belong to
        // another process
        let pgid = Pid::from_raw(-self.pid);
        if reused(self.pid, self.start_time) {
            debug!("Not signaling process {}, its PID got reused", self.name);
        } else {
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
        }

        let deadline = Instant::now() + self.stop_timeout;
        if !self.wait_exit(self.stop_timeout) {
            // The leader did not exit, so its PID is still the same
            debug!(
                "Process {} (via {}) did not exit after SIGTERM, sending SIGKILL",
                self.name, self.command,
            );
            let _ = kill(pgid, Signal::SIGKILL);
        } else if !wait_group_exit(self.pid, self.start_time, deadline) {
            // The PID of the leader cannot be reused while its group has
            // members
            debug!(
                "Process group of {} (via {}) did not exit after SIGTERM, sending SIGKILL",
                self.name, self.command,
            );
            let _ = kill(pgid, Signal::SIGKILL);
        }
        let joined = self.watch.take().map(JoinHandle::join);

        // Keep the group tracked as long as it has members, so that a force
        // quit or the next start kills them.
        if wait_group_exit(self.pid, self.start_time, Instant::now() + KILL_TIMEOUT) {
            untrack(self.pid);
        } else {
            debug!(
                "Process group of {} (via {}) still has members after SIGKILL",
                self.name, self.command,
            );
        }
        if joined.is_some_and(|j| j.is_err()) {
            bail!(
                "Unable to stop process {} (via {})",
                self.name,
                self.command
            );
        }
        debug!("Process {} (via {}) stopped", self.name, self.command);
        Ok(())
    }

    fn died(&mut self) -> bool {
        self.watch.is_some() && self.flags.died.load(Ordering::SeqCst)
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
        let mut p = Process::start(d.path(), "test", "echo", &["hello", "world"])?;

        // The executed command is recorded with its resolved path
        let run: Run = serde_json::from_str(&fs::read_to_string(d.path().join("run.json"))?)?;
        assert_eq!(run.command, System::find_executable("echo")?);
        assert_eq!(run.args, ["hello", "world"]);

        // The process exits on its own, which counts as died, and its
        // output ends up in the log file named after the command
        let deadline = Instant::now() + Duration::from_secs(5);
        while !p.died() {
            assert!(Instant::now() < deadline, "process did not exit");
            sleep(Duration::from_millis(10));
        }
        assert_eq!(
            fs::read_to_string(d.path().join("echo.log"))?,
            "hello world\n"
        );
        Ok(())
    }

    #[test]
    fn start_without_terminal_input() -> Result<()> {
        let d = tempdir()?;
        let mut p = Process::start(d.path(), "", "sh", &["-c", "cat; echo done"])?;
        p.readiness_timeout = 5;
        p.wait_ready("done")?;
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
    fn start_with_env_success() -> Result<()> {
        let d = tempdir()?;
        let mut p = Process::start_with_env(
            d.path(),
            "",
            "sh",
            &["-c", "echo \"env $KUBERNIX_TEST_VALUE\""],
            &[("KUBERNIX_TEST_VALUE", "set")],
        )?;
        p.wait_ready("env set")?;
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
    fn wait_ready_canceled() -> Result<()> {
        let d = tempdir()?;
        let mut p = Process::start(d.path(), "", "sleep", &["500"])?;
        let flag = Arc::new(AtomicBool::new(true));
        set_cancel(Arc::clone(&flag));
        let err = p.wait_ready("never").unwrap_err();
        flag.store(false, Ordering::SeqCst);
        assert!(err.downcast_ref::<Canceled>().is_some(), "{:#}", err);
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
    fn stop_kills_remaining_group_members() -> Result<()> {
        let d = tempdir()?;
        // The leader exits on SIGTERM, but leaves a member behind which
        // ignores it.
        let mut p = Process::start(
            d.path(),
            "",
            "sh",
            &["-c", "(trap '' TERM; echo ready; exec sleep 500) & wait"],
        )?;
        p.readiness_timeout = 5;
        p.stop_timeout = Duration::from_millis(500);
        p.wait_ready("ready")?;
        let pid = p.pid;
        p.stop()?;
        assert_eq!(kill(Pid::from_raw(-pid), None), Err(Errno::ESRCH));
        assert!(!running_groups().iter().any(|(p, _)| *p == pid));
        Ok(())
    }

    #[test]
    fn running_groups_tracks_processes() -> Result<()> {
        let tracked = |pid| running_groups().iter().any(|(p, _)| *p == pid);
        let d = tempdir()?;
        let mut p = Process::start(d.path(), "", "sleep", &["500"])?;
        let pid = p.pid;
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
    fn stop_skips_reused_pid() -> Result<()> {
        let d = tempdir()?;
        let mut p = Process::start(d.path(), "", "sleep", &["500"])?;
        let start = p.start_time.context("no start time")?;
        assert!(!reused(p.pid, Some(start)));
        assert!(reused(p.pid, Some(start + 1)));
        assert!(!reused(p.pid, None));
        p.stop()?;
        Ok(())
    }

    #[test]
    fn kill_leftovers_of_saved_groups() -> Result<()> {
        let d = tempdir()?;
        let file = d.path().join("groups.json");
        let mut child = command("sleep").arg("500").spawn()?;
        let pid = i32::try_from(child.id())?;
        let start = ProcStat::read(pid).context("no stat")?.start_time;

        // Groups of another boot are ignored
        let saved = |boot_id: String| SavedGroups {
            boot_id,
            groups: vec![(pid, Some(start))],
        };
        fs::write(&file, serde_json::to_vec(&saved("other".into()))?)?;
        kill_leftovers(&file);
        assert!(child.try_wait()?.is_none());

        fs::write(
            &file,
            serde_json::to_vec(&saved(boot_id().unwrap_or_default()))?,
        )?;
        kill_leftovers(&file);
        let deadline = Instant::now() + Duration::from_secs(5);
        while child.try_wait()?.is_none() {
            assert!(Instant::now() < deadline, "leftover not killed");
            sleep(POLL_INTERVAL);
        }

        // Missing and invalid files are fine
        kill_leftovers(&d.path().join("missing"));
        fs::write(&file, "invalid")?;
        kill_leftovers(&file);
        Ok(())
    }

    #[test]
    fn groups_snapshot_only_persistent_ones() -> Result<()> {
        let d = tempdir()?;
        let file = d.path().join("groups.json");
        let mut groups = Groups {
            groups: BTreeMap::from([
                (
                    1,
                    Group {
                        start: Some(2),
                        persist: true,
                    },
                ),
                (
                    3,
                    Group {
                        start: None,
                        persist: false,
                    },
                ),
            ]),
            file: None,
            generation: 7,
        };
        assert_eq!(groups.snapshot(), None);

        groups.file = Some(file.clone());
        let snapshot = groups.snapshot().context("no snapshot")?;
        assert_eq!(
            snapshot,
            Snapshot {
                file: file.clone(),
                generation: 7,
                groups: vec![(1, Some(2))],
            }
        );
        snapshot.save();
        let saved: SavedGroups = serde_json::from_slice(&fs::read(&file)?)?;
        assert_eq!(saved.groups, vec![(1, Some(2))]);
        assert_eq!(Some(saved.boot_id), boot_id());

        groups.groups.remove(&1);
        groups.snapshot().context("no snapshot")?.save();
        assert!(!file.exists());
        Ok(())
    }

    #[test]
    fn run_cancelable_output() -> Result<()> {
        let output = run_cancelable(
            command("echo").arg("hello").stdout(Stdio::piped()),
            None,
            || false,
        )?;
        assert!(output.status.success());
        assert_eq!(String::from_utf8_lossy(&output.stdout), "hello\n");

        // Output larger than the pipe buffers and input
        let output = run_cancelable(
            command("sh")
                .args(["-c", "cat; head -c 1000000 /dev/zero"])
                .stdout(Stdio::piped()),
            Some(b"in"),
            || false,
        )?;
        assert_eq!(output.stdout.len(), 1_000_002);
        assert!(output.stdout.starts_with(b"in"));
        Ok(())
    }

    #[test]
    fn run_cancelable_stops_on_cancel() {
        let start = Instant::now();
        let err = run_cancelable(command("sleep").arg("30"), None, || {
            start.elapsed() > Duration::from_millis(200)
        })
        .unwrap_err();
        assert_eq!(err.to_string(), "Canceled");
        assert!(start.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn find_processes_by_command_line() -> Result<()> {
        let sh = System::find_executable("sh")?;
        let marker = format!("kubernix-test-{}", std::process::id());
        // The shell forks sleep, so kill its whole process group afterwards
        let mut child = Command::new(&sh)
            .args(["-c", "sleep 30; true", &marker])
            .process_group(0)
            .spawn()?;
        let pid = i32::try_from(child.id())?;
        let found = find_processes(&sh, &marker);
        assert_eq!(found.iter().map(|(p, _)| *p).collect::<Vec<_>>(), [pid]);
        assert!(find_processes(Path::new("/other"), &marker).is_empty());

        signal_processes(&found, Signal::SIGKILL);
        kill(Pid::from_raw(-pid), Signal::SIGKILL).ok();
        child.wait()?;
        Ok(())
    }

    #[test]
    fn cmdline_containing() {
        let cmdline = b"/nix/bin/conmonrs\0--runtime-dir\0/root/crio/node-0/run\0";
        let names = &["conmon", "conmonrs"];
        assert!(cmdline_contains(cmdline, names, "/root/crio/node-0"));
        assert!(cmdline_contains(cmdline, names, "/root/crio/node-0/"));
        assert!(cmdline_contains(cmdline, names, "/root/crio/node-0/run"));
        assert!(!cmdline_contains(cmdline, names, "/root/crio/node-1"));
        assert!(!cmdline_contains(cmdline, &["conmon"], "/root/crio/node-0"));
        assert!(!cmdline_contains(cmdline, &[], "/root/crio/node-0"));
        assert!(!cmdline_contains(cmdline, names, ""));
        assert!(!cmdline_contains(cmdline, names, "/"));
        assert!(!cmdline_contains(b"", names, "/root"));

        // After a separator
        let cmdline = b"/bin/containerd-shim-runc-v2\0-address=/r/containerd/node-1/c.sock\0";
        let names = &["containerd-shim-runc-v2"];
        assert!(cmdline_contains(cmdline, names, "/r/containerd/node-1"));

        // node-1 is not node-10
        let cmdline = b"/nix/bin/conmonrs\0--runtime-dir\0/root/crio/node-10/run\0";
        assert!(!cmdline_contains(
            cmdline,
            &["conmonrs"],
            "/root/crio/node-1"
        ));
        let cmdline = b"/nix/bin/conmonrs\0--runtime-dir=/root/crio/node-10\0";
        assert!(!cmdline_contains(
            cmdline,
            &["conmonrs"],
            "/root/crio/node-1"
        ));

        // A run directory which ends with this one, or starts with it
        let cmdline = b"/nix/bin/conmonrs\0--runtime-dir\0/other/root/crio/node-1/run\0";
        assert!(!cmdline_contains(
            cmdline,
            &["conmonrs"],
            "/root/crio/node-1"
        ));
        let cmdline = b"/nix/bin/conmonrs\0--runtime-dir\0/root-2/crio/node-1/run\0";
        assert!(!cmdline_contains(cmdline, &["conmonrs"], "/root"));
    }

    #[test]
    fn cmdline_matching() {
        let exe = Path::new("/bin/kubelet");
        assert!(cmdline_matches(
            b"/bin/kubelet\0--a\0--config=x\0",
            exe,
            "--config=x"
        ));
        assert!(!cmdline_matches(
            b"/bin/kubelet\0--config=xy\0",
            exe,
            "--config=x"
        ));
        assert!(!cmdline_matches(
            b"podman\0/bin/kubelet\0--config=x\0",
            exe,
            "--config=x"
        ));
        assert!(!cmdline_matches(b"", exe, "--config=x"));
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
    fn proc_stat_alive() -> Result<()> {
        let pid = i32::try_from(std::process::id())?;
        let start = ProcStat::read(pid).context("no stat")?.start_time;
        assert!(ProcStat::alive_since(pid, start));
        // A reused PID
        assert!(!ProcStat::alive_since(pid, start + 1));
        assert!(!ProcStat::alive_since(i32::MAX, start));
        Ok(())
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

    #[test]
    fn stop_exec_without_processes() -> Result<()> {
        let d = tempdir()?;
        let mut p = Process::start(d.path(), "", "sleep", &["500"])?;
        p.stop_exec(Path::new("/nonexistent"), "--none");
        assert!(p.flags.stopping.load(Ordering::SeqCst));
        p.stop()?;
        assert!(!p.died());
        Ok(())
    }
}
