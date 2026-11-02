//! The lock of a run directory.
//!
//! The lock is an open file description lock on `kubernix.lock`, which
//! contains the PID of the process which took it. The lock stays held
//! while kubernix execs into its next stage (`nix develop` and the
//! rootless session), since the next stage inherits the file descriptor
//! and adopts the lock instead of taking a new one. Other processes can
//! check the lock without taking it.

use crate::process::ProcStat;
use anyhow::{Context, Result};
use nix::{
    errno::Errno,
    fcntl::{FcntlArg, FdFlag, fcntl},
    libc,
};
use std::{
    fs::{self, File, OpenOptions},
    io::{ErrorKind, Write},
    mem::ManuallyDrop,
    os::{
        fd::{AsRawFd, FromRawFd, RawFd},
        unix::fs::MetadataExt,
    },
    path::Path,
    process::{Command, id},
};

/// The environment variable which passes the file descriptor of the lock
/// to the next stage.
const FD_ENV: &str = "KUBERNIX_LOCK_FD";

/// An exclusive lock of the run directory, released once the file
/// descriptor gets closed in all processes which inherited it.
pub(crate) struct RunLock {
    /// The lock file, unless an ancestor holds the lock
    file: Option<File>,
}

impl RunLock {
    /// Take the lock of `path`, or adopt the lock inherited from the
    /// previous stage. Returns `None` if another process holds the lock.
    pub fn acquire(path: &Path) -> Result<Option<Self>> {
        let inherited = std::env::var(FD_ENV).ok().and_then(|fd| fd.parse().ok());
        Self::acquire_with(path, inherited)
    }

    fn acquire_with(path: &Path, inherited: Option<RawFd>) -> Result<Option<Self>> {
        if let Some(lock) = inherited.and_then(|fd| Self::adopt(path, fd)) {
            return Ok(Some(lock));
        }

        // rootlesskit does not pass the descriptor to its child, but keeps
        // the lock held until the child exits.
        if inherited.is_some()
            && let Some(Some(holder)) = Self::holder(path)?
            && ProcStat::is_ancestor(holder)
        {
            return Ok(Some(Self { file: None }));
        }

        let mut file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(path)
            .with_context(|| format!("Unable to open lock file '{}'", path.display()))?;
        match fcntl(&file, FcntlArg::F_OFD_SETLK(&Self::flock(libc::F_WRLCK))) {
            Ok(_) => {}
            Err(Errno::EAGAIN | Errno::EACCES) => return Ok(None),
            Err(e) => {
                return Err(e).with_context(|| format!("Unable to lock '{}'", path.display()));
            }
        }
        file.set_len(0)
            .and_then(|()| write!(file, "{}", id()))
            .with_context(|| format!("Unable to write lock file '{}'", path.display()))?;
        Ok(Some(Self { file: Some(file) }))
    }

    /// Adopt the inherited file descriptor if it refers to the lock file.
    fn adopt(path: &Path, fd: RawFd) -> Option<Self> {
        if fd < 0 {
            return None;
        }
        let expected = fs::metadata(path).ok()?;
        // SAFETY: F_GETFD only checks whether the descriptor is open.
        if unsafe { libc::fcntl(fd, libc::F_GETFD) } < 0 {
            return None;
        }
        // SAFETY: the descriptor is open, and the file is not dropped, so
        // that a foreign descriptor stays open.
        let actual = ManuallyDrop::new(unsafe { File::from_raw_fd(fd) })
            .metadata()
            .ok()?;
        if actual.ino() != expected.ino() || actual.dev() != expected.dev() {
            return None;
        }
        // SAFETY: the descriptor is open, refers to the lock file and is
        // owned by nobody else in this process, since it got inherited.
        let file = unsafe { File::from_raw_fd(fd) };
        // Do not pass it to the components, and take the lock again,
        // which succeeds if the open file description already holds it.
        fcntl(&file, FcntlArg::F_SETFD(FdFlag::FD_CLOEXEC)).ok()?;
        fcntl(&file, FcntlArg::F_OFD_SETLK(&Self::flock(libc::F_WRLCK))).ok()?;
        Some(Self { file: Some(file) })
    }

    /// Pass the lock to the command, which replaces the current process.
    pub fn inherit(&self, cmd: &mut Command) -> Result<()> {
        let fd = match &self.file {
            Some(file) => {
                fcntl(file, FcntlArg::F_SETFD(FdFlag::empty()))
                    .context("Unable to pass the lock file to the next stage")?;
                file.as_raw_fd()
            }
            // An ancestor holds the lock
            None => -1,
        };
        cmd.env(FD_ENV, fd.to_string());
        Ok(())
    }

    /// Remove the variable which passes the lock from the command
    /// environment, for commands which are not a stage of kubernix.
    pub fn forget(cmd: &mut Command) {
        cmd.env_remove(FD_ENV);
    }

    /// Check if a process holds the lock of `path` without taking it.
    /// Returns `None` if the lock is free, otherwise the PID of the process
    /// which took it, if known.
    pub fn holder(path: &Path) -> Result<Option<Option<i32>>> {
        let file = match File::open(path) {
            Ok(file) => file,
            Err(e) if e.kind() == ErrorKind::NotFound => return Ok(None),
            Err(e) => {
                return Err(e)
                    .with_context(|| format!("Unable to open lock file '{}'", path.display()));
            }
        };
        let mut flock = Self::flock(libc::F_WRLCK);
        fcntl(&file, FcntlArg::F_OFD_GETLK(&mut flock))
            .with_context(|| format!("Unable to check the lock '{}'", path.display()))?;
        if i32::from(flock.l_type) == libc::F_UNLCK {
            return Ok(None);
        }
        Ok(Some(
            fs::read_to_string(path)
                .ok()
                .and_then(|pid| pid.trim().parse().ok()),
        ))
    }

    /// A lock description for the whole file.
    fn flock(lock_type: libc::c_int) -> libc::flock {
        // SAFETY: flock is a plain C struct, for which all zero is valid.
        let mut flock: libc::flock = unsafe { std::mem::zeroed() };
        flock.l_type = lock_type as libc::c_short;
        flock.l_whence = libc::SEEK_SET as libc::c_short;
        flock
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::{
        thread::sleep,
        time::{Duration, Instant},
    };
    use tempfile::tempdir;

    /// Wait until the lock is free. Child processes which other tests fork
    /// concurrently hold a copy of the descriptor until they exec.
    pub(crate) fn released(path: &Path) -> Result<bool> {
        let deadline = Instant::now() + Duration::from_secs(5);
        while RunLock::holder(path)?.is_some() {
            if Instant::now() >= deadline {
                return Ok(false);
            }
            sleep(Duration::from_millis(10));
        }
        Ok(true)
    }

    fn file(lock: &RunLock) -> Result<&File> {
        lock.file.as_ref().context("no lock file")
    }

    /// A duplicate of the descriptor, which shares the open file
    /// description like an inherited descriptor after exec.
    fn dup(file: &File) -> Result<RawFd> {
        Ok(fcntl(file, FcntlArg::F_DUPFD_CLOEXEC(0))?)
    }

    #[test]
    fn acquire_is_exclusive() -> Result<()> {
        let dir = tempdir()?;
        let path = dir.path().join("lock");
        assert_eq!(RunLock::holder(&path)?, None);

        let lock = RunLock::acquire_with(&path, None)?.context("not acquired")?;
        assert!(RunLock::acquire_with(&path, None)?.is_none());
        assert_eq!(RunLock::holder(&path)?, Some(Some(i32::try_from(id())?)));

        drop(lock);
        assert!(released(&path)?);
        assert!(RunLock::acquire_with(&path, None)?.is_some());
        Ok(())
    }

    #[test]
    fn acquire_adopts_inherited_lock() -> Result<()> {
        let dir = tempdir()?;
        let path = dir.path().join("lock");
        let lock = RunLock::acquire_with(&path, None)?.context("not acquired")?;
        let adopted =
            RunLock::acquire_with(&path, Some(dup(file(&lock)?)?))?.context("not adopted")?;

        drop(lock);
        assert!(RunLock::holder(&path)?.is_some());
        drop(adopted);
        assert!(released(&path)?);
        Ok(())
    }

    #[test]
    fn acquire_ignores_foreign_fd() -> Result<()> {
        let dir = tempdir()?;
        let path = dir.path().join("lock");
        let other = File::create(dir.path().join("other"))?;
        let lock = RunLock::acquire_with(&path, Some(other.as_raw_fd()))?;
        assert!(lock.is_some());
        // The foreign descriptor is still open
        assert!(other.metadata().is_ok());

        // Closed and invalid descriptors are ignored as well
        drop(lock);
        assert!(released(&path)?);
        assert!(RunLock::acquire_with(&path, Some(-1))?.is_some());
        Ok(())
    }

    #[test]
    fn inherit_passes_descriptor() -> Result<()> {
        let dir = tempdir()?;
        let lock =
            RunLock::acquire_with(&dir.path().join("lock"), None)?.context("not acquired")?;
        let flags = || -> Result<FdFlag> {
            Ok(FdFlag::from_bits_truncate(fcntl(
                file(&lock)?,
                FcntlArg::F_GETFD,
            )?))
        };
        assert!(flags()?.contains(FdFlag::FD_CLOEXEC));

        let mut cmd = Command::new("true");
        lock.inherit(&mut cmd)?;
        assert!(!flags()?.contains(FdFlag::FD_CLOEXEC));
        let fd = file(&lock)?.as_raw_fd().to_string();
        assert!(
            cmd.get_envs()
                .any(|(k, v)| k == FD_ENV && v == Some(fd.as_ref()))
        );

        RunLock::forget(&mut cmd);
        assert!(cmd.get_envs().any(|(k, v)| k == FD_ENV && v.is_none()));
        Ok(())
    }
}
