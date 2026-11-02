//! Host state of the cluster in root mode.
//!
//! The sysctls of the host, the rules of kube-proxy and the bridges are
//! global. Only one cluster runs in root mode at a time: the components
//! listen on fixed ports (see `Network::check_ports`), and the state file
//! stays locked from the setup of the system until the host state got
//! restored, which also covers the startup of a cluster before its
//! components listen and its cleanup. The original values get saved before
//! the setup and restored when the cluster stops.

use crate::files::{PRIVATE, open_lock_file, open_nofollow, try_lock, write_atomic};
use anyhow::{Context, Result};
use log::{debug, warn};
use nix::libc;
use std::{
    collections::BTreeMap,
    fs::{self, File},
    io::{self, Read},
    path::{Path, PathBuf},
};

/// The state file, locked via `<state file>.lock`. `/run` gets cleared on
/// reboot, like the sysctls.
pub const HOST_STATE_FILE: &str = "/run/kubernix-host-state";

/// The location of the sysctls.
pub const PROC_SYS: &str = "/proc/sys";

/// The path of the sysctl `key` below `proc_sys`, usually `PROC_SYS`.
pub fn sysctl_path(proc_sys: &Path, key: &str) -> PathBuf {
    proc_sys.join(key.replace('.', "/"))
}

/// Read the value of the sysctl `key` below `proc_sys`.
pub fn read_sysctl(proc_sys: &Path, key: &str) -> io::Result<String> {
    Ok(fs::read_to_string(sysctl_path(proc_sys, key))?
        .trim()
        .to_string())
}

/// Set the sysctl `key` below `proc_sys` to `value`.
pub fn write_sysctl(proc_sys: &Path, key: &str, value: &str) -> io::Result<()> {
    fs::write(sysctl_path(proc_sys, key), value)
}

/// The sysctls which kubernix, the kubelet and kube-proxy change in root
/// mode and which get restored. Sysctls which do not exist before the
/// setup, like the ones of modules which get loaded, are not restored.
///
/// `net.ipv4.ip_forward` and the `net.bridge.bridge-nf-call-*` sysctls are
/// not restored, like kubeadm and kind do not: Docker, libvirt or a VPN
/// started while the cluster runs may need them, and disabling the
/// forwarding resets it for every interface. `br_netfilter` stays loaded
/// as well.
const SYSCTLS: &[&str] = &[
    "net.ipv4.conf.all.route_localnet",
    "net.netfilter.nf_conntrack_max",
    "net.netfilter.nf_conntrack_tcp_timeout_close_wait",
    "net.netfilter.nf_conntrack_tcp_timeout_established",
    "kernel.keys.root_maxbytes",
    "kernel.keys.root_maxkeys",
    "kernel.panic",
    "kernel.panic_on_oops",
    "vm.overcommit_memory",
    "vm.panic_on_oom",
];

/// The host state, locked exclusively while the cluster starts, runs and
/// stops.
///
/// The state file contains the original values of the sysctls as
/// `key=value` lines, other lines get ignored. It gets replaced atomically,
/// so a crash while writing it never loses the saved values. The lock is
/// an open file description lock of a separate lock file next to it
/// (`<state file>.lock`), which stays the same file while the state file
/// gets replaced, and gets released when the process exits, also after a
/// crash or force quit. The original values stay in the state file then,
/// so the next cluster keeps them and restores them on stop.
pub struct HostState {
    path: PathBuf,
    lock: File,
    proc_sys: PathBuf,
    released: bool,
}

impl HostState {
    /// Lock the state file `path` exclusively without waiting. Returns
    /// `None` if another cluster holds the lock.
    pub fn acquire(path: &Path, proc_sys: &Path) -> Result<Option<Self>> {
        let lock_path = Self::lock_path(path)?;
        let lock = open_lock_file(&lock_path)?;
        if !try_lock(&lock, libc::F_WRLCK)
            .with_context(|| format!("Unable to lock '{}'", lock_path.display()))?
        {
            return Ok(None);
        }
        Ok(Some(Self {
            path: path.into(),
            lock,
            proc_sys: proc_sys.into(),
            released: false,
        }))
    }

    /// The lock file of the state file `path`.
    fn lock_path(path: &Path) -> Result<PathBuf> {
        let name = path
            .file_name()
            .with_context(|| format!("Invalid host state file '{}'", path.display()))?;
        let mut lock = name.to_os_string();
        lock.push(".lock");
        Ok(path.with_file_name(lock))
    }

    /// Read the saved sysctls. A missing file contains none.
    fn read(&self) -> Result<BTreeMap<String, String>> {
        let mut content = String::new();
        match open_nofollow(&self.path) {
            Ok(mut file) => {
                file.read_to_string(&mut content)
                    .with_context(|| format!("Unable to read '{}'", self.path.display()))?;
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => {
                return Err(e).with_context(|| format!("Unable to open '{}'", self.path.display()));
            }
        }
        Ok(content
            .lines()
            .filter_map(|line| line.split_once('='))
            .map(|(k, v)| (k.into(), v.into()))
            .collect())
    }

    /// Save the current values of the sysctls which are not saved yet.
    /// Saved values are original ones, which a former run saved before
    /// changing them and did not restore, for example after a crash.
    pub fn save(&self) -> Result<()> {
        let mut saved = self.read().context("Unable to read the host state")?;
        let mut changed = false;
        for key in SYSCTLS {
            if saved.contains_key(*key) {
                continue;
            }
            if let Ok(value) = read_sysctl(&self.proc_sys, key) {
                saved.insert(key.to_string(), value);
                changed = true;
            }
        }
        if changed {
            let content = saved
                .iter()
                .map(|(k, v)| format!("{}={}\n", k, v))
                .collect::<String>();
            self.write(&content)
                .context("Unable to save the host state")?;
        }
        Ok(())
    }

    /// Replace the state file atomically, which only root may access.
    fn write(&self, content: &str) -> Result<()> {
        write_atomic(&self.path, content.as_bytes(), PRIVATE, None)
    }

    /// Restore the saved sysctls, which did not change on failure. Saved
    /// values of sysctls which are not restored, like the ones which former
    /// builds saved before a crash, are skipped. The values which could not
    /// be restored stay in the state file, so that a later run retries,
    /// unless the sysctl does not exist anymore, for example since its
    /// module got unloaded.
    fn restore(&self) -> Result<()> {
        let mut failed = String::new();
        for (key, value) in self.read()? {
            if !SYSCTLS.contains(&key.as_str()) {
                debug!("Not restoring sysctl '{}'", key);
                continue;
            }
            if read_sysctl(&self.proc_sys, &key).is_ok_and(|x| x == value) {
                continue;
            }
            if !sysctl_path(&self.proc_sys, &key).exists() {
                debug!("Not restoring sysctl '{}', it does not exist", key);
                continue;
            }
            debug!("Restoring sysctl '{}' to '{}'", key, value);
            if let Err(e) = write_sysctl(&self.proc_sys, &key, &value) {
                warn!(
                    "Unable to restore sysctl '{}' to '{}', keeping it for the next run: {}",
                    key, value, e
                );
                failed.push_str(&format!("{}={}\n", key, value));
            }
        }
        self.write(&failed)
    }

    /// Restore the saved sysctls, run `cleanup` to restore the rest of the
    /// host state and unlock the state file, once.
    pub fn release(&mut self, cleanup: impl FnOnce()) {
        if self.released {
            return;
        }
        self.released = true;
        debug!("Restoring the host state");
        if let Err(e) = self.restore() {
            warn!("Unable to restore the host state: {:#}", e);
        }
        cleanup();
        // Closing the file releases the lock as well, but child processes
        // may share the open file description between fork and exec
        if let Err(e) = try_lock(&self.lock, libc::F_UNLCK) {
            debug!("Unable to unlock the host state: {}", e);
        }
    }
}

impl Drop for HostState {
    /// Restore the state on errors before the cluster started.
    fn drop(&mut self) {
        self.release(|| {});
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::bail;
    use tempfile::{TempDir, tempdir};

    /// A state file and a fake `/proc/sys` with some of the sysctls.
    fn setup() -> Result<(TempDir, PathBuf, PathBuf)> {
        let dir = tempdir()?;
        let file = dir.path().join("state");
        let proc_sys = dir.path().join("sys");
        for (key, value) in [
            ("net.ipv4.ip_forward", "0"),
            ("net.ipv4.conf.all.route_localnet", "0"),
            ("kernel.panic", "0"),
        ] {
            let path = proc_sys.join(key.replace('.', "/"));
            fs::create_dir_all(path.parent().context("no parent")?)?;
            fs::write(path, format!("{}\n", value))?;
        }
        Ok((dir, file, proc_sys))
    }

    fn get(proc_sys: &Path, key: &str) -> Result<String> {
        Ok(read_sysctl(proc_sys, key)?)
    }

    fn set(proc_sys: &Path, key: &str, value: &str) -> Result<()> {
        let path = sysctl_path(proc_sys, key);
        fs::create_dir_all(path.parent().context("no parent")?)?;
        Ok(write_sysctl(proc_sys, key, value)?)
    }

    #[test]
    fn sysctl_read_write() -> Result<()> {
        let (_dir, _, proc_sys) = setup()?;
        assert_eq!(
            sysctl_path(&proc_sys, "net.ipv4.ip_forward"),
            proc_sys.join("net/ipv4/ip_forward")
        );
        assert_eq!(read_sysctl(&proc_sys, "net.ipv4.ip_forward")?, "0");
        write_sysctl(&proc_sys, "net.ipv4.ip_forward", "1")?;
        assert_eq!(read_sysctl(&proc_sys, "net.ipv4.ip_forward")?, "1");
        assert!(read_sysctl(&proc_sys, "net.ipv4.missing").is_err());
        assert!(write_sysctl(&proc_sys, "net.missing.key", "1").is_err());
        Ok(())
    }

    /// Change the sysctls like a cluster does, loading `br_netfilter`
    /// adds the bridge sysctls.
    fn change(proc_sys: &Path) -> Result<()> {
        set(proc_sys, "net.ipv4.ip_forward", "1")?;
        set(proc_sys, "net.ipv4.conf.all.route_localnet", "1")?;
        set(proc_sys, "kernel.panic", "10")?;
        set(proc_sys, "net.bridge.bridge-nf-call-iptables", "1")
    }

    fn start(file: &Path, proc_sys: &Path) -> Result<HostState> {
        let Some(state) = HostState::acquire(file, proc_sys)? else {
            bail!("host state locked");
        };
        state.save()?;
        change(proc_sys)?;
        state.save()?;
        Ok(state)
    }

    #[test]
    fn restore_on_release() -> Result<()> {
        let (_dir, file, proc_sys) = setup()?;
        let mut state = start(&file, &proc_sys)?;
        let saved = fs::read_to_string(&file)?;
        assert_eq!(
            saved,
            "kernel.panic=0\nnet.ipv4.conf.all.route_localnet=0\n"
        );

        let mut called = false;
        state.release(|| called = true);
        assert!(called);
        assert_eq!(get(&proc_sys, "net.ipv4.conf.all.route_localnet")?, "0");
        assert_eq!(get(&proc_sys, "kernel.panic")?, "0");
        // Other services may need the forwarding and the bridge filtering
        assert_eq!(get(&proc_sys, "net.ipv4.ip_forward")?, "1");
        assert_eq!(get(&proc_sys, "net.bridge.bridge-nf-call-iptables")?, "1");
        assert_eq!(fs::read_to_string(&file)?, "");
        // Only once
        called = false;
        state.release(|| called = true);
        assert!(!called);
        Ok(())
    }

    #[test]
    fn exclusive_until_released() -> Result<()> {
        let (_dir, file, proc_sys) = setup()?;
        let mut first = start(&file, &proc_sys)?;
        // Another cluster can neither start nor change the saved values
        assert!(HostState::acquire(&file, &proc_sys)?.is_none());
        let mut second = None;
        first.release(|| {
            second = HostState::acquire(&file, &proc_sys).ok().flatten();
        });
        assert!(second.is_none(), "locked while restoring");
        // Unlocked before the file gets closed
        let mut second = start(&file, &proc_sys)?;
        assert!(HostState::acquire(&file, &proc_sys)?.is_none());
        drop(first);
        second.release(|| {});
        assert_eq!(get(&proc_sys, "net.ipv4.conf.all.route_localnet")?, "0");
        Ok(())
    }

    #[test]
    fn keep_original_values_after_crash() -> Result<()> {
        let (_dir, file, proc_sys) = setup()?;
        // A crash releases the lock without restoring the state
        let mut crashed = start(&file, &proc_sys)?;
        crashed.released = true;
        // Closing the file releases the lock as well, but child processes
        // which other tests fork concurrently may hold a copy of it
        try_lock(&crashed.lock, libc::F_UNLCK)?;
        drop(crashed);

        let mut state = start(&file, &proc_sys)?;
        state.release(|| {});
        assert_eq!(get(&proc_sys, "net.ipv4.conf.all.route_localnet")?, "0");
        assert_eq!(get(&proc_sys, "kernel.panic")?, "0");
        Ok(())
    }

    #[test]
    fn state_file_replaced_while_locked() -> Result<()> {
        use std::os::unix::fs::MetadataExt;
        let (dir, file, proc_sys) = setup()?;
        assert_eq!(HostState::lock_path(&file)?, dir.path().join("state.lock"));
        // The format of former runs gets read as well
        fs::write(&file, "kernel.panic=5\n")?;
        let inode = fs::metadata(&file)?.ino();
        let mut state = start(&file, &proc_sys)?;
        let metadata = fs::metadata(&file)?;
        assert_ne!(metadata.ino(), inode, "replaced instead of truncated");
        assert_eq!(metadata.mode() & 0o777, 0o600);
        assert_eq!(
            fs::read_to_string(&file)?,
            "kernel.panic=5\nnet.ipv4.conf.all.route_localnet=0\n"
        );
        // The lock stays held although the state file got replaced
        assert!(HostState::acquire(&file, &proc_sys)?.is_none());
        state.release(|| {});
        assert_eq!(get(&proc_sys, "kernel.panic")?, "5");

        // Without a state file, nothing gets restored
        fs::remove_file(&file)?;
        let mut state = HostState::acquire(&file, &proc_sys)?.context("locked")?;
        assert!(state.read()?.is_empty());
        state.release(|| {});
        let mut entries = fs::read_dir(dir.path())?
            .map(|e| Ok(e?.file_name().to_string_lossy().into_owned()))
            .collect::<Result<Vec<_>>>()?;
        entries.sort();
        assert_eq!(entries, ["state", "state.lock", "sys"]);
        Ok(())
    }

    #[test]
    fn restore_on_drop() -> Result<()> {
        let (_dir, file, proc_sys) = setup()?;
        drop(start(&file, &proc_sys)?);
        assert_eq!(get(&proc_sys, "net.ipv4.conf.all.route_localnet")?, "0");
        assert!(HostState::acquire(&file, &proc_sys)?.is_some());
        Ok(())
    }

    #[test]
    fn keep_values_which_failed_to_restore() -> Result<()> {
        let (_dir, file, proc_sys) = setup()?;
        let mut state = start(&file, &proc_sys)?;
        // Writing a directory fails
        let panic = sysctl_path(&proc_sys, "kernel.panic");
        fs::remove_file(&panic)?;
        fs::create_dir(&panic)?;
        state.release(|| {});
        assert_eq!(get(&proc_sys, "net.ipv4.conf.all.route_localnet")?, "0");
        assert_eq!(fs::read_to_string(&file)?, "kernel.panic=0\n");

        // The next run keeps the original value and restores it
        fs::remove_dir(&panic)?;
        set(&proc_sys, "kernel.panic", "10")?;
        let mut state = start(&file, &proc_sys)?;
        state.release(|| {});
        assert_eq!(get(&proc_sys, "kernel.panic")?, "0");
        assert_eq!(fs::read_to_string(&file)?, "");

        // Sysctls which do not exist anymore are dropped
        let mut state = start(&file, &proc_sys)?;
        fs::remove_file(&panic)?;
        state.release(|| {});
        assert_eq!(fs::read_to_string(&file)?, "");
        Ok(())
    }

    #[test]
    fn skip_values_saved_by_former_builds() -> Result<()> {
        let (_dir, file, proc_sys) = setup()?;
        fs::write(
            &file,
            "net.ipv4.ip_forward=0\nnet.bridge.bridge-nf-call-iptables=0\ninvalid\n",
        )?;
        let mut state = start(&file, &proc_sys)?;
        state.release(|| {});
        assert_eq!(get(&proc_sys, "net.ipv4.ip_forward")?, "1");
        assert_eq!(get(&proc_sys, "net.bridge.bridge-nf-call-iptables")?, "1");
        assert_eq!(get(&proc_sys, "kernel.panic")?, "0");
        Ok(())
    }
}
