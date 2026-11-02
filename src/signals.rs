//! Termination signal handling.
//!
//! The cluster components run in their own sessions, so they never see
//! terminal signals. Kubernix registers flags for SIGTERM, SIGHUP and
//! SIGINT instead of dying, so that the bootstrap and wait loops can abort
//! into the regular cleanup path.

use anyhow::{Result, bail};
use signal_hook::{
    consts::signal::{SIGHUP, SIGINT, SIGTERM},
    flag,
};
use std::sync::{
    Arc, OnceLock,
    atomic::{AtomicBool, Ordering},
};

struct Flags {
    /// Set on SIGTERM and SIGHUP
    terminate: Arc<AtomicBool>,
    /// Set on SIGINT
    interrupt: Arc<AtomicBool>,
}

static FLAGS: OnceLock<Flags> = OnceLock::new();

fn flags() -> &'static Flags {
    FLAGS.get_or_init(|| Flags {
        terminate: Arc::new(AtomicBool::new(false)),
        interrupt: Arc::new(AtomicBool::new(false)),
    })
}

/// Register the signal handlers. Afterwards the signals no longer
/// terminate the process, but only set the flags.
pub fn register() -> Result<()> {
    let f = flags();
    flag::register(SIGTERM, Arc::clone(&f.terminate))?;
    flag::register(SIGHUP, Arc::clone(&f.terminate))?;
    flag::register(SIGINT, Arc::clone(&f.interrupt))?;
    Ok(())
}

/// Returns true if SIGTERM or SIGHUP has been received.
pub fn terminated() -> bool {
    flags().terminate.load(Ordering::Relaxed)
}

/// Returns true if any termination signal has been received.
pub fn requested() -> bool {
    terminated() || flags().interrupt.load(Ordering::Relaxed)
}

/// Forget a received SIGINT, for example while an interactive shell runs
/// in the foreground and the interrupt is meant for it.
pub fn clear_interrupt() {
    flags().interrupt.store(false, Ordering::Relaxed);
}

/// Forget all received signals, for example once the cleanup started.
pub fn reset() {
    flags().terminate.store(false, Ordering::Relaxed);
    clear_interrupt();
}

/// Fail if a termination signal has been received.
pub fn check() -> Result<()> {
    if requested() {
        bail!("Received termination signal, aborting")
    }
    Ok(())
}
