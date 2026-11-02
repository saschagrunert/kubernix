//! Termination signal handling.
//!
//! The cluster components and helper commands run in their own process
//! groups, so they never see terminal signals. Kubernix handles SIGTERM,
//! SIGQUIT, SIGHUP and SIGINT instead of dying, so that the bootstrap and
//! wait loops can abort into the regular cleanup path.
//!
//! Another signal requests a force quit, except for SIGHUP. Signals within
//! one second after the first one are treated as duplicates of it: in
//! rootless mode, rootlesskit forwards the SIGINT of the terminal which
//! kubernix receives as well, and on an SSH hangup both the kernel and the
//! login shell send SIGHUP.

use anyhow::{Result, bail};
use log::debug;
use signal_hook::{
    consts::signal::{SIGHUP, SIGINT, SIGQUIT, SIGTERM},
    iterator::Signals,
};
use std::{
    os::raw::c_int,
    sync::{
        Arc, LazyLock, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

/// Signals within this time after the first one are duplicates of it.
const DUPLICATE_WINDOW: Duration = Duration::from_secs(1);

#[derive(Default)]
struct State {
    /// Set on the first signal
    requested: AtomicBool,
    /// Set on another signal after the duplicate window
    forced: AtomicBool,
    /// Set while an interactive shell runs in the foreground, which owns
    /// the interrupts
    shell: AtomicBool,
    /// The time of the first signal
    first: Mutex<Option<Instant>>,
}

impl State {
    fn record(&self, signal: c_int, now: Instant) {
        if signal == SIGINT && self.shell.load(Ordering::SeqCst) {
            debug!("Ignoring SIGINT, it is meant for the shell");
            return;
        }
        let Ok(mut first) = self.first.lock() else {
            return;
        };
        match *first {
            None => {
                *first = Some(now);
                self.requested.store(true, Ordering::SeqCst);
            }
            Some(t) if signal != SIGHUP && now.saturating_duration_since(t) >= DUPLICATE_WINDOW => {
                self.forced.store(true, Ordering::SeqCst);
            }
            Some(_) => debug!("Ignoring signal {}, it does not force quit", signal),
        }
    }
}

static STATE: LazyLock<State> = LazyLock::new(State::default);

/// Register the signal handlers. Afterwards the signals no longer
/// terminate the process, but are recorded by a background thread.
pub fn register() -> Result<()> {
    let mut signals = Signals::new([SIGTERM, SIGQUIT, SIGHUP, SIGINT])?;
    thread::Builder::new()
        .name("signals".into())
        .spawn(move || {
            for signal in signals.forever() {
                STATE.record(signal, Instant::now());
            }
        })?;
    Ok(())
}

/// Keep running on SIGHUP with a handler which does nothing. Unlike an
/// ignored signal, a handler gets reset on exec, so child processes still
/// terminate on SIGHUP.
pub fn survive_hangup() -> Result<()> {
    signal_hook::flag::register(SIGHUP, Arc::new(AtomicBool::new(false)))?;
    Ok(())
}

/// Returns true if a termination signal has been received.
pub fn requested() -> bool {
    STATE.requested.load(Ordering::SeqCst)
}

/// Returns true if another termination signal requested a force quit.
pub fn forced() -> bool {
    STATE.forced.load(Ordering::SeqCst)
}

/// Ignore SIGINT while an interactive shell runs in the foreground, the
/// interrupts of the terminal are meant for it.
pub fn set_shell(running: bool) {
    STATE.shell.store(running, Ordering::SeqCst);
}

/// Fail if a termination signal has been received.
pub fn check() -> Result<()> {
    if requested() {
        bail!("Received termination signal, aborting")
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{os::unix::process::ExitStatusExt, process::Command};

    #[test]
    fn hangup_handler_not_inherited() -> Result<()> {
        survive_hangup()?;
        let status = Command::new("sh")
            .args(["-c", "kill -HUP $$; exit 0"])
            .status()?;
        assert_eq!(status.signal(), Some(SIGHUP));
        Ok(())
    }

    #[test]
    fn second_signal_forces_quit() {
        let s = State::default();
        let t = Instant::now();
        s.record(SIGINT, t);
        assert!(s.requested.load(Ordering::SeqCst));
        assert!(!s.forced.load(Ordering::SeqCst));
        s.record(SIGTERM, t + DUPLICATE_WINDOW);
        assert!(s.forced.load(Ordering::SeqCst));
    }

    #[test]
    fn duplicates_and_hangups_do_not_force_quit() {
        let s = State::default();
        let t = Instant::now();
        s.record(SIGHUP, t);
        assert!(s.requested.load(Ordering::SeqCst));
        s.record(SIGINT, t + Duration::from_millis(500));
        s.record(SIGHUP, t + Duration::from_secs(5));
        assert!(!s.forced.load(Ordering::SeqCst));
        s.record(SIGQUIT, t + Duration::from_secs(5));
        assert!(s.forced.load(Ordering::SeqCst));
    }

    #[test]
    fn interrupts_ignored_while_shell_runs() {
        let s = State::default();
        let t = Instant::now();
        s.shell.store(true, Ordering::SeqCst);
        s.record(SIGINT, t);
        assert!(!s.requested.load(Ordering::SeqCst));
        s.record(SIGTERM, t);
        assert!(s.requested.load(Ordering::SeqCst));
        s.shell.store(false, Ordering::SeqCst);
        s.record(SIGINT, t + DUPLICATE_WINDOW);
        assert!(s.forced.load(Ordering::SeqCst));
    }
}
