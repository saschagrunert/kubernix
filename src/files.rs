//! Safe writes of the files in the run root.
//!
//! In root mode, kubernix writes keys, kubeconfigs, configurations and logs
//! into the run root as root. Files get replaced atomically: the content
//! goes into a new temporary file next to the target, which then replaces
//! the target via rename. The temporary file is created exclusively and no
//! path gets followed if it is a symbolic link, so that a planted link
//! cannot redirect a write, and a crash never leaves a partially written
//! file behind.
//!
//! Lock files carry open file description locks of the whole file, which
//! get released once the last descriptor of the open file description is
//! closed, also after a crash.

use anyhow::{Context, Result, bail};
use nix::{
    errno::Errno,
    fcntl::{FcntlArg, fcntl},
    libc,
    unistd::{Gid, Uid, fchown},
};
use std::{
    fs::{self, DirBuilder, File, Metadata, OpenOptions, Permissions},
    io::{self, Read, Write},
    os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt},
    path::Path,
    process,
    sync::atomic::{AtomicU64, Ordering},
};

/// The mode of files which everybody may read.
const PUBLIC: u32 = 0o644;

/// The mode of files which only their owner may read and write.
pub(crate) const PRIVATE: u32 = 0o600;

/// The user and group which own a file.
pub(crate) type Owner = (Uid, Gid);

/// Atomically write `content` to `path` with `mode`, owned by `owner` or
/// the current user.
pub(crate) fn write_atomic(
    path: &Path,
    content: &[u8],
    mode: u32,
    owner: Option<Owner>,
) -> Result<()> {
    write_atomic_with(path, content, mode, owner, true)
}

/// Atomically write `content` to `path`, which everybody may read, without
/// syncing it to disk. After a power loss, the file may be empty, so only
/// use it for files which are best effort.
pub(crate) fn write_unsynced(path: &Path, content: &[u8]) -> Result<()> {
    write_atomic_with(path, content, PUBLIC, None, false)
}

/// Atomically write `content` to `path`, syncing it to disk before it
/// replaces the target if `sync` is set.
fn write_atomic_with(
    path: &Path,
    content: &[u8],
    mode: u32,
    owner: Option<Owner>,
    sync: bool,
) -> Result<()> {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let name = path
        .file_name()
        .with_context(|| format!("Invalid file path '{}'", path.display()))?;
    let tmp = path.with_file_name(format!(
        ".{}.{}-{}.tmp",
        name.to_string_lossy(),
        process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed),
    ));
    let write = || -> io::Result<()> {
        // A leftover of a crashed process with the same PID
        if let Err(e) = fs::remove_file(&tmp)
            && e.kind() != io::ErrorKind::NotFound
        {
            return Err(e);
        }
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(mode)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&tmp)?;
        // The umask may have removed bits of the mode
        file.set_permissions(Permissions::from_mode(mode))?;
        if let Some((uid, gid)) = owner {
            fchown(&file, Some(uid), Some(gid))?;
        }
        file.write_all(content)?;
        if sync {
            file.sync_all()?;
        }
        fs::rename(&tmp, path)
    };
    write()
        .inspect_err(|_| {
            let _ = fs::remove_file(&tmp);
        })
        .with_context(|| format!("Unable to write '{}'", path.display()))
}

/// Atomically write `content` to `path`, which everybody may read.
pub(crate) fn write_file(path: &Path, content: &[u8]) -> Result<()> {
    write_atomic(path, content, PUBLIC, None)
}

/// Atomically write `content` to `path`, which only its owner can read,
/// for example because it contains a private key.
pub(crate) fn write_private(path: &Path, content: &[u8]) -> Result<()> {
    write_atomic(path, content, PRIVATE, None)
}

/// Write `content` to `path` only if the file does not exist or its
/// current contents differ. Avoids unnecessary filesystem writes and
/// inode updates on warm restarts.
pub(crate) fn write_if_changed(path: &Path, content: &str) -> Result<()> {
    if !unchanged(path, content.as_bytes(), |_| true) {
        write_file(path, content.as_bytes())?;
    }
    Ok(())
}

/// Write `content` to `path`, which only `owner` (or the current user)
/// can read, if the file does not exist, its contents differ or its mode
/// or owner do not match. Returns true if the file has been written.
pub(crate) fn write_private_if_changed(
    path: &Path,
    content: &[u8],
    owner: Option<Owner>,
) -> Result<bool> {
    let (uid, gid) = owner.unwrap_or_else(|| (Uid::effective(), Gid::effective()));
    let fits = |m: &Metadata| {
        m.mode() & 0o7777 == PRIVATE && m.uid() == uid.as_raw() && m.gid() == gid.as_raw()
    };
    if unchanged(path, content, fits) {
        return Ok(false);
    }
    write_atomic(path, content, PRIVATE, owner)?;
    Ok(true)
}

/// Returns true if `path` is a regular file with `content`, whose metadata
/// fits. Symbolic links never match.
fn unchanged(path: &Path, content: &[u8], fits: impl FnOnce(&Metadata) -> bool) -> bool {
    let read = || -> io::Result<bool> {
        let mut file = open_nofollow(path)?;
        let metadata = file.metadata()?;
        if !metadata.is_file() || metadata.len() != content.len() as u64 || !fits(&metadata) {
            return Ok(false);
        }
        let mut existing = Vec::with_capacity(content.len());
        file.read_to_end(&mut existing)?;
        Ok(existing == content)
    };
    read().unwrap_or(false)
}

/// Open `path` for reading, which fails if it is a symbolic link.
pub(crate) fn open_nofollow(path: &Path) -> io::Result<File> {
    OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
}

/// Create or truncate the file at `path` for writing, which everybody may
/// read, for example a log file. Fails if `path` is a symbolic link.
pub(crate) fn create_file(path: &Path) -> Result<File> {
    OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(PUBLIC)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
        .with_context(|| format!("Unable to create '{}'", path.display()))
}

/// Open the lock file `path`, which gets created if needed and only its
/// owner may access. Fails if `path` is a symbolic link.
pub(crate) fn open_lock_file(path: &Path) -> Result<File> {
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .mode(PRIVATE)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
        .with_context(|| format!("Unable to open lock file '{}'", path.display()))?;
    // The umask does not widen the mode, but older versions may have
    // created the file readable by others
    if file.metadata()?.mode() & 0o077 != 0 {
        file.set_permissions(Permissions::from_mode(PRIVATE))
            .with_context(|| format!("Unable to restrict '{}'", path.display()))?;
    }
    Ok(file)
}

/// The description of a lock of the whole file, of the type `F_RDLCK`,
/// `F_WRLCK` or `F_UNLCK`.
pub(crate) fn whole_file_lock(lock_type: libc::c_int) -> libc::flock {
    // SAFETY: flock is a plain C struct, for which all zero is valid.
    let mut flock: libc::flock = unsafe { std::mem::zeroed() };
    flock.l_type = lock_type as libc::c_short;
    flock.l_whence = libc::SEEK_SET as libc::c_short;
    flock
}

/// Take (`F_WRLCK` or `F_RDLCK`) or release (`F_UNLCK`) the open file
/// description lock of the whole file without waiting. Returns false if
/// another open file description holds a conflicting lock. Taking a lock
/// which the open file description holds already succeeds.
pub(crate) fn try_lock(file: &File, lock_type: libc::c_int) -> nix::Result<bool> {
    match fcntl(file, FcntlArg::F_OFD_SETLK(&whole_file_lock(lock_type))) {
        Ok(_) => Ok(true),
        Err(Errno::EAGAIN | Errno::EACCES) => Ok(false),
        Err(e) => Err(e),
    }
}

/// Create the directory `path` and its parents, where only the owner can
/// access `path`. Fixes the mode of an existing directory. Fails if `path`
/// is a symbolic link, no directory or not owned by the current user, so
/// that a planted link cannot redirect the private files.
pub(crate) fn create_dir_private(path: &Path) -> Result<()> {
    let context = || format!("Unable to create directory '{}'", path.display());
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(parent)
            .with_context(context)?;
    }
    match DirBuilder::new().mode(0o700).create(path) {
        Err(e) if e.kind() != io::ErrorKind::AlreadyExists => {
            return Err(e).with_context(context);
        }
        _ => {}
    }
    let dir = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
        .open(path)
        .with_context(|| {
            format!(
                "Unable to open '{}', it has to be a directory and no symbolic link",
                path.display()
            )
        })?;
    let metadata = dir.metadata().with_context(context)?;
    let uid = Uid::effective().as_raw();
    if metadata.uid() != uid {
        bail!(
            "Directory '{}' is owned by UID {} instead of {}",
            path.display(),
            metadata.uid(),
            uid
        )
    }
    if metadata.mode() & 0o7777 != 0o700 {
        dir.set_permissions(Permissions::from_mode(0o700))
            .with_context(|| format!("Unable to restrict '{}'", path.display()))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;
    use tempfile::tempdir;

    fn mode(path: &Path) -> Result<u32> {
        Ok(fs::symlink_metadata(path)?.mode() & 0o7777)
    }

    /// The directory contains only the provided entries, no temporary files.
    fn assert_entries(dir: &Path, expected: &[&str]) -> Result<()> {
        let mut entries = fs::read_dir(dir)?
            .map(|e| Ok(e?.file_name().to_string_lossy().into_owned()))
            .collect::<Result<Vec<_>>>()?;
        entries.sort();
        assert_eq!(entries, expected);
        Ok(())
    }

    #[test]
    fn write_file_and_private_modes() -> Result<()> {
        let dir = tempdir()?;
        let public = dir.path().join("public");
        let private = dir.path().join("private");
        write_file(&public, b"a")?;
        write_private(&private, b"b")?;
        assert_eq!(fs::read(&public)?, b"a");
        assert_eq!(mode(&public)?, 0o644);
        assert_eq!(fs::read(&private)?, b"b");
        assert_eq!(mode(&private)?, 0o600);

        // Existing files get the new mode as well
        write_private(&public, b"c")?;
        assert_eq!(mode(&public)?, 0o600);
        assert_entries(dir.path(), &["private", "public"])
    }

    #[test]
    fn write_atomic_replaces_symlink() -> Result<()> {
        let dir = tempdir()?;
        let target = dir.path().join("target");
        let link = dir.path().join("link");
        fs::write(&target, "secret")?;
        symlink(&target, &link)?;

        write_private(&link, b"new")?;
        assert_eq!(fs::read(&target)?, b"secret");
        assert!(fs::symlink_metadata(&link)?.is_file());
        assert_eq!(fs::read(&link)?, b"new");
        assert_entries(dir.path(), &["link", "target"])
    }

    #[test]
    fn write_atomic_failure_leaves_nothing() -> Result<()> {
        let dir = tempdir()?;
        // A directory cannot be replaced by a file
        fs::create_dir(dir.path().join("dir"))?;
        assert!(write_file(&dir.path().join("dir"), b"a").is_err());
        assert!(write_file(&dir.path().join("missing").join("file"), b"a").is_err());
        assert_entries(dir.path(), &["dir"])
    }

    #[test]
    fn write_if_changed_creates_new_file() -> Result<()> {
        let dir = tempdir()?;
        let path = dir.path().join("file");
        write_if_changed(&path, "hello")?;
        assert_eq!(fs::read_to_string(&path)?, "hello");
        Ok(())
    }

    #[test]
    fn write_if_changed_skips_identical() -> Result<()> {
        let dir = tempdir()?;
        let path = dir.path().join("file");
        fs::write(&path, "hello")?;
        let inode = fs::metadata(&path)?.ino();
        write_if_changed(&path, "hello")?;
        assert_eq!(fs::metadata(&path)?.ino(), inode);
        Ok(())
    }

    #[test]
    fn write_if_changed_updates_different() -> Result<()> {
        let dir = tempdir()?;
        let path = dir.path().join("file");
        fs::write(&path, "old")?;
        write_if_changed(&path, "new")?;
        assert_eq!(fs::read_to_string(&path)?, "new");
        Ok(())
    }

    #[test]
    fn write_if_changed_replaces_symlink_with_same_content() -> Result<()> {
        let dir = tempdir()?;
        let target = dir.path().join("target");
        let link = dir.path().join("link");
        fs::write(&target, "hello")?;
        symlink(&target, &link)?;
        write_if_changed(&link, "hello")?;
        assert!(fs::symlink_metadata(&link)?.is_file());
        Ok(())
    }

    #[test]
    fn write_private_if_changed_fixes_mode() -> Result<()> {
        let dir = tempdir()?;
        let path = dir.path().join("file");
        assert!(write_private_if_changed(&path, b"a", None)?);
        assert!(!write_private_if_changed(&path, b"a", None)?);
        let owner = (Uid::effective(), Gid::effective());
        assert!(!write_private_if_changed(&path, b"a", Some(owner))?);
        assert!(write_private_if_changed(&path, b"b", None)?);

        fs::set_permissions(&path, Permissions::from_mode(0o644))?;
        assert!(write_private_if_changed(&path, b"b", None)?);
        assert_eq!(mode(&path)?, 0o600);
        Ok(())
    }

    #[test]
    fn create_file_refuses_symlink() -> Result<()> {
        let dir = tempdir()?;
        let target = dir.path().join("target");
        let link = dir.path().join("link");
        fs::write(&target, "secret")?;
        symlink(&target, &link)?;
        assert!(create_file(&link).is_err());
        assert_eq!(fs::read(&target)?, b"secret");

        let log = dir.path().join("log");
        fs::write(&log, "old")?;
        create_file(&log)?.write_all(b"new")?;
        assert_eq!(fs::read(&log)?, b"new");
        Ok(())
    }

    #[test]
    fn write_unsynced_replaces_file() -> Result<()> {
        let dir = tempdir()?;
        let path = dir.path().join("file");
        write_unsynced(&path, b"a")?;
        write_unsynced(&path, b"b")?;
        assert_eq!(fs::read(&path)?, b"b");
        assert_eq!(mode(&path)?, 0o644);
        assert_entries(dir.path(), &["file"])
    }

    #[test]
    fn lock_file_is_exclusive_and_private() -> Result<()> {
        let dir = tempdir()?;
        let path = dir.path().join("lock");
        fs::write(&path, "")?;
        fs::set_permissions(&path, Permissions::from_mode(0o644))?;
        let first = open_lock_file(&path)?;
        assert_eq!(mode(&path)?, 0o600);
        let second = open_lock_file(&path)?;

        assert!(try_lock(&first, libc::F_WRLCK)?);
        // Again for the same open file description
        assert!(try_lock(&first, libc::F_WRLCK)?);
        assert!(!try_lock(&second, libc::F_WRLCK)?);
        assert!(!try_lock(&second, libc::F_RDLCK)?);
        assert!(try_lock(&first, libc::F_UNLCK)?);
        assert!(try_lock(&second, libc::F_WRLCK)?);

        // Symbolic links are not followed
        let link = dir.path().join("link");
        symlink(&path, &link)?;
        assert!(open_lock_file(&link).is_err());
        Ok(())
    }

    #[test]
    fn create_dir_private_fixes_mode() -> Result<()> {
        let dir = tempdir()?;
        let path = dir.path().join("a").join("b");
        create_dir_private(&path)?;
        assert_eq!(mode(&path)?, 0o700);
        fs::set_permissions(&path, Permissions::from_mode(0o755))?;
        create_dir_private(&path)?;
        assert_eq!(mode(&path)?, 0o700);
        Ok(())
    }

    #[test]
    fn create_dir_private_refuses_symlink() -> Result<()> {
        let dir = tempdir()?;
        let target = dir.path().join("target");
        fs::create_dir(&target)?;
        fs::set_permissions(&target, Permissions::from_mode(0o755))?;
        let link = dir.path().join("pki");
        symlink(&target, &link)?;
        assert!(create_dir_private(&link).is_err());
        assert_eq!(mode(&target)?, 0o755);
        assert!(fs::symlink_metadata(&link)?.file_type().is_symlink());

        // A dangling link does not get followed either
        let dangling = dir.path().join("dangling");
        symlink(dir.path().join("missing"), &dangling)?;
        assert!(create_dir_private(&dangling).is_err());
        assert!(!dir.path().join("missing").exists());

        // Neither does a file
        let file = dir.path().join("file");
        fs::write(&file, "")?;
        assert!(create_dir_private(&file).is_err());
        Ok(())
    }
}
