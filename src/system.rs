//! System preparation and host utilities.
//!
//! Handles kernel module loading, sysctl configuration, the host state
//! of the cluster in root mode (see `HostState`), hosts file
//! management, executable lookup in `$PATH`, and container detection.

use crate::{
    Config, crio,
    hoststate::{self, HOST_STATE_FILE, HostState, PROC_SYS},
    node::Node,
    process,
    proxy::Proxy,
    rootless::Rootless,
};
use anyhow::{Context, Result, bail};
use log::{debug, info, warn};
use nix::mount::{MsFlags, mount};
use std::{
    env::{split_paths, var, var_os},
    ffi::OsStr,
    fmt::Display,
    fs::{self, read_to_string},
    net::Ipv4Addr,
    path::{Path, PathBuf},
};

/// Manages host-level system preparation and cleanup.
pub struct System {
    /// The hosts file which contains node entries to be removed on cleanup.
    hosts: Option<PathBuf>,
    /// Comment which marks the hosts file lines of the run root.
    hosts_tag: String,
    /// The locked host state in root mode, restored on cleanup.
    host_state: Option<HostState>,
}

impl System {
    /// Host paths recreated inside the rootlesskit mount namespace, because
    /// the copy-up keeps them owned by the host root and not writable.
    /// `/var/lib/cni` holds the CNI result cache of CRI-O and containerd,
    /// which is not configurable, `/run/netns` the pod network namespaces
    /// of containerd, `/run/containerd` the shim sockets and `/run/kubernix`
    /// the temporary version files and exec PID files of CRI-O.
    const ROOTLESS_DIRS: &'static [&'static str] = &[
        "/var/lib/kubelet",
        "/var/lib/crio",
        "/var/lib/cni",
        "/var/lib/containers",
        "/var/cache/containers",
        "/var/log/containers",
        "/var/log/crio",
        "/run/lock",
        "/run/containers",
        "/run/containerd",
        "/run/netns",
        crio::RUN_BASE,
    ];

    /// The kubelet device plugin directory, which does not follow `--root-dir`.
    pub const DEVICE_PLUGINS_DIR: &'static str = "/var/lib/kubelet/device-plugins";

    /// Prepare the rootless mount namespace for one kubelet per node.
    ///
    /// Every kubelet binds its socket in `DEVICE_PLUGINS_DIR`, so each one
    /// runs in its own mount namespace with a node directory mounted over
    /// it. The kubelet directory of the run root gets shared propagation, so
    /// that the volume mounts of the kubelets are still visible to the CRI
    /// runtime, while the device plugin directory becomes a private mount
    /// point, so that the node directories mounted over it stay within their
    /// namespace.
    fn isolate_device_plugins_dir(kubelet_dir: &Path) -> Result<()> {
        let none = None::<&str>;
        for (dir, propagation) in [
            (kubelet_dir, MsFlags::MS_SHARED),
            (Path::new(Self::DEVICE_PLUGINS_DIR), MsFlags::MS_PRIVATE),
        ] {
            fs::create_dir_all(dir)
                .with_context(|| format!("Unable to create {}", dir.display()))?;
            mount(Some(dir), dir, none, MsFlags::MS_BIND, none)
                .with_context(|| format!("Unable to bind mount {}", dir.display()))?;
            mount(none, dir, none, propagation, none).with_context(|| {
                format!("Unable to change the propagation of {}", dir.display())
            })?;
        }
        Ok(())
    }

    /// Create a new system
    pub fn setup(config: &Config) -> Result<Self> {
        if config.is_rootless() {
            // The rootless re-exec always enters a user namespace, the
            // kubelets and CRI runtimes rely on the setup below.
            if !Self::in_user_namespace() {
                bail!("Rootless mode requires to run inside a user namespace")
            }
            // Inside rootlesskit, pre-existing host directories under the
            // copy-up overlay retain host root ownership (mapped to nobody
            // in the user namespace) and are not writable. Recreate paths
            // that kubernix components write to under rootlesskit copy-ups.
            // When adding a new component that writes to a host path under
            // /var or /run, add the path to ROOTLESS_DIRS.
            for dir in Self::ROOTLESS_DIRS {
                let path = PathBuf::from(dir);
                if (path.exists() || path.is_symlink())
                    && let Err(e) = fs::remove_dir_all(&path)
                {
                    warn!("Unable to remove '{}': {}", dir, e);
                }
                fs::create_dir_all(&path)
                    .with_context(|| format!("Unable to create rootless directory '{}'", dir))?;
            }
            if config.multi_node() {
                Self::isolate_device_plugins_dir(&config.root().join("kubelet"))?;
            } else {
                fs::create_dir_all(Self::DEVICE_PLUGINS_DIR)
                    .with_context(|| format!("Unable to create {}", Self::DEVICE_PLUGINS_DIR))?;
            }

            // On NixOS, /etc/hosts is a symlink into the read-only nix
            // store. Replace it with a regular file so multi-node can
            // write host entries.
            let hosts_path = Self::hosts();
            if hosts_path.is_symlink() {
                let content = read_to_string(&hosts_path).unwrap_or_default();
                fs::remove_file(&hosts_path).context("Unable to remove hosts symlink")?;
                fs::write(&hosts_path, content).context("Unable to write hosts file")?;
            }

            // Kernel modules cannot be loaded from the user namespace.
            Self::enable_network_sysctls(true)?;
            Rootless::warn_missing_controllers();
        }

        // The state of the user namespace of rootlesskit gets removed with
        // it, while root mode restores the sysctls and removes the rules of
        // kube-proxy when the cluster stops. The lock of the host state
        // keeps other clusters in root mode from changing the host, like
        // removing the bridges, until then, also before the components
        // listen on their ports.
        let host_state = if config.is_rootless() {
            None
        } else {
            let state = match HostState::acquire(Path::new(HOST_STATE_FILE), Path::new(PROC_SYS)) {
                Ok(Some(state)) => Some(state),
                Ok(None) => bail!(
                    "Another cluster in root mode is running, starting or stopping, \
                     see `kubernix status --root <dir>`"
                ),
                Err(e) => {
                    warn!(
                        "{:#}, the sysctls and kube-proxy rules will not be restored",
                        e
                    );
                    None
                }
            };
            let save = || state.as_ref().map_or(Ok(()), HostState::save);
            // Neither works in a user namespace, like a rootless container
            if Self::in_container()? || Self::in_user_namespace() {
                info!("Skipping modprobe and sysctl (containerized)");
            } else {
                // Save the sysctls before loading the modules, and the ones
                // which the modules add afterwards
                save()?;
                for module in &["overlay", "br_netfilter", "ip_conntrack"] {
                    Self::modprobe(module)?;
                }
                save()?;
                Self::enable_network_sysctls(false)?;
            }
            state
        };

        // Remove the entries of a former run of the same root which did not
        // clean up, other roots may belong to running clusters
        let hosts_file = Self::hosts();
        let hosts_tag = Self::hosts_tag(config.root());
        if let Err(e) = Self::remove_hosts_entries(&hosts_file, &hosts_tag) {
            debug!("Unable to remove stale hosts entries: {:#}", e);
        }

        let hosts = if config.multi_node() {
            // Try to write the hostnames, which does not work on every
            // system, like on NixOS in root mode, where the hosts file is
            // a link into the Nix store. The API server reaches the
            // kubelets via their node IP anyway.
            let entries = (0..config.nodes())
                .map(|x| format!("{} {}", Ipv4Addr::LOCALHOST, Node::raw(x)))
                .collect::<Vec<_>>();
            match Self::add_hosts_entries(&hosts_file, &hosts_tag, &entries) {
                Err(e) => {
                    warn!("{:#}, the node names do not resolve on the host", e);
                    None
                }
                _ => Some(hosts_file),
            }
        } else {
            None
        };

        Ok(Self {
            hosts,
            hosts_tag,
            host_state,
        })
    }

    /// Returns true if running inside a user namespace (uid 0 maps to
    /// a non-zero host UID). Used to guard destructive host path cleanup
    /// and to detect root inside a user namespace at startup.
    ///
    /// This function is intentionally side-effect-free (no logging)
    /// because it may be called before the logger is initialized.
    pub(crate) fn in_user_namespace() -> bool {
        read_to_string("/proc/self/uid_map")
            .map(|s| {
                s.lines().any(|line| {
                    let mut fields = line.split_whitespace();
                    fields.next() == Some("0")
                        && fields.next().is_some_and(|host_uid| host_uid != "0")
                })
            })
            .unwrap_or(false)
    }

    /// Returns true if the process is running inside a container
    pub fn in_container() -> Result<bool> {
        if Path::new("/.dockerenv").exists() || Path::new("/run/.containerenv").exists() {
            return Ok(true);
        }
        Ok(read_to_string(Path::new("/proc/1/cgroup"))
            .context("Unable to retrieve systems container status")?
            .lines()
            .any(|x| {
                x.contains("libpod")
                    || x.contains("podman")
                    || x.contains("docker")
                    || x.contains("containerd")
                    || x.contains("lxc")
            }))
    }

    /// The restore of the hosts file, which can run in another thread
    /// while the regular cleanup still runs, for example to force quit.
    pub(crate) fn hosts_restore(&self) -> impl FnOnce() + Send + 'static {
        let hosts = self.hosts.clone();
        let tag = self.hosts_tag.clone();
        move || {
            if let Some(hosts) = hosts
                && let Err(e) = Self::remove_hosts_entries(&hosts, &tag)
            {
                warn!(
                    "Unable to restore hosts file, may need manual cleanup: {:#}",
                    e
                );
            }
        }
    }

    /// Restore the initial system state. The components are stopped
    /// already, so the rules of kube-proxy can be removed.
    fn cleanup(&mut self) {
        if let Some(hosts) = self.hosts.take()
            && let Err(e) = Self::remove_hosts_entries(&hosts, &self.hosts_tag)
        {
            warn!(
                "Unable to restore hosts file, may need manual cleanup: {:#}",
                e
            );
        }
        if let Some(mut state) = self.host_state.take() {
            state.release(Proxy::cleanup_rules);
        }
    }

    /// Short and stable identifier of the run root (32 bit FNV-1a hash).
    pub fn root_id(root: &Path) -> String {
        let hash = root
            .as_os_str()
            .as_encoded_bytes()
            .iter()
            .fold(0x811c_9dc5_u32, |hash, byte| {
                (hash ^ u32::from(*byte)).wrapping_mul(0x0100_0193)
            });
        format!("{:08x}", hash)
    }

    /// Comment which marks the hosts file lines added for the run root.
    fn hosts_tag(root: &Path) -> String {
        format!("# kubernix {}", Self::root_id(root))
    }

    /// Append the entries marked with `tag` to the hosts file, replacing
    /// lines with the same tag and untagged duplicates of the entries,
    /// which former versions did not tag.
    fn add_hosts_entries(path: &Path, tag: &str, entries: &[String]) -> Result<()> {
        let mut content = Self::strip_hosts_entries(&Self::read_hosts(path)?, tag, entries);
        if !content.is_empty() && !content.ends_with('\n') {
            content.push('\n');
        }
        for entry in entries {
            content.push_str(&format!("{} {}\n", entry, tag));
        }
        fs::write(path, content)
            .with_context(|| format!("Unable to write hosts file '{}'", path.display()))
    }

    /// Remove all lines marked with `tag` from the hosts file, the file
    /// stays untouched if there are none.
    fn remove_hosts_entries(path: &Path, tag: &str) -> Result<()> {
        let content = Self::read_hosts(path)?;
        let stripped = Self::strip_hosts_entries(&content, tag, &[]);
        if stripped != content {
            fs::write(path, stripped)
                .with_context(|| format!("Unable to write hosts file '{}'", path.display()))?;
        }
        Ok(())
    }

    fn read_hosts(path: &Path) -> Result<String> {
        read_to_string(path)
            .with_context(|| format!("Unable to read hosts file '{}'", path.display()))
    }

    /// Remove the lines marked with `tag` and the lines equal to one of
    /// `untagged`, keep all others including their line endings.
    fn strip_hosts_entries(content: &str, tag: &str, untagged: &[String]) -> String {
        content
            .split_inclusive('\n')
            .filter(|line| {
                let line = line.trim_end();
                !line.ends_with(tag) && !untagged.iter().any(|x| x == line)
            })
            .collect()
    }

    /// Find an executable inside the current $PATH environment
    pub fn find_executable<P>(name: P) -> Result<PathBuf>
    where
        P: AsRef<Path> + Display,
    {
        Self::find_executable_in(name, var_os("PATH").as_deref())
    }

    /// Find an executable inside the provided `PATH`
    pub fn find_executable_in<P>(name: P, path: Option<&OsStr>) -> Result<PathBuf>
    where
        P: AsRef<Path> + Display,
    {
        path.and_then(|paths| {
            split_paths(paths)
                .filter_map(|dir| {
                    let full_path = dir.join(&name);
                    if full_path.is_file() {
                        Some(full_path)
                    } else {
                        None
                    }
                })
                .next()
        })
        .with_context(|| format!("Unable to find executable '{}' in $PATH", name))
    }

    /// Return the full path to the default system shell
    pub fn shell() -> Result<String> {
        let shell = var("SHELL").unwrap_or_else(|_| "sh".into());
        Ok(Self::find_executable(&shell)
            .with_context(|| format!("Unable to find system shell '{}'", shell))?
            .display()
            .to_string())
    }

    /// Check if a kernel module is already loaded
    fn module_loaded(module: &str) -> bool {
        read_to_string(PathBuf::from("/proc/modules"))
            .map(|content| {
                content
                    .lines()
                    .any(|l| l.split_whitespace().next() == Some(module))
            })
            .unwrap_or(false)
    }

    /// Load a single kernel module via 'modprobe', skipping if already loaded
    fn modprobe(module: &str) -> Result<()> {
        if Self::module_loaded(module) {
            debug!("Kernel module '{}' already loaded, skipping", module);
            return Ok(());
        }
        debug!("Loading kernel module '{}'", module);
        let output = process::command("modprobe").arg(module).output()?;
        if !output.status.success() {
            bail!(
                "Unable to load '{}' kernel module: {}",
                module,
                String::from_utf8(output.stderr)?,
            );
        }
        Ok(())
    }

    /// Check if a sysctl is already enabled (value is "1")
    fn sysctl_enabled(key: &str) -> bool {
        hoststate::read_sysctl(Path::new(PROC_SYS), key).is_ok_and(|v| v == "1")
    }

    /// Enable the network sysctls. In rootless mode, they belong to the
    /// network namespace of rootlesskit, which the user namespace owns.
    /// Failures are not fatal there, for example outside of the user
    /// namespace or with a read-only `/proc/sys`.
    fn enable_network_sysctls(rootless: bool) -> Result<()> {
        let mut sysctls = vec!["net.ipv4.conf.all.route_localnet", "net.ipv4.ip_forward"];
        // The bridge sysctls only exist with br_netfilter loaded, see
        // Proxy::render for rootless mode without it.
        if Path::new("/proc/sys/net/bridge").exists() {
            sysctls.extend([
                "net.bridge.bridge-nf-call-ip6tables",
                "net.bridge.bridge-nf-call-iptables",
            ]);
        } else {
            debug!("Skipping bridge sysctls, br_netfilter is not loaded");
        }
        for sysctl in sysctls {
            match Self::sysctl_enable(sysctl) {
                Err(e) if rootless => warn!("{:#}", e),
                result => result?,
            }
        }
        Ok(())
    }

    /// Enable a single sysctl by setting it to '1', skipping if already set
    fn sysctl_enable(key: &str) -> Result<()> {
        if Self::sysctl_enabled(key) {
            debug!("Sysctl '{}' already enabled, skipping", key);
            return Ok(());
        }
        debug!("Enabling sysctl '{}'", key);
        hoststate::write_sysctl(Path::new(PROC_SYS), key, "1")
            .with_context(|| format!("Unable to set sysctl '{}=1'", key))
    }

    fn hosts() -> PathBuf {
        PathBuf::from("/etc/hosts")
    }
}

impl Drop for System {
    /// Restore the system state on any exit path, including errors before
    /// the cluster has been bootstrapped.
    fn drop(&mut self) {
        self.cleanup();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const VALID_EXECUTABLE: &str = "echo";
    const INVALID_EXECUTABLE: &str = "should-not-exist";

    #[test]
    fn module_failure() {
        assert!(System::modprobe("invalid").is_err());
    }

    #[test]
    fn sysctl_failure() {
        assert!(System::sysctl_enable("invalid").is_err());
    }

    const HOSTS: &str = "127.0.0.1 localhost\n::1 localhost # comment\n\n# end\n";
    const TAG_A: &str = "# kubernix 0000000a";
    const TAG_B: &str = "# kubernix 0000000b";

    fn hosts_file(content: &str) -> Result<tempfile::NamedTempFile> {
        let file = tempfile::NamedTempFile::new()?;
        fs::write(file.path(), content)?;
        Ok(file)
    }

    fn entries() -> Vec<String> {
        vec!["127.0.0.1 node-0".into(), "127.0.0.1 node-1".into()]
    }

    #[test]
    fn root_id_is_stable() {
        assert_eq!(System::root_id(Path::new("")), "811c9dc5");
        assert_eq!(System::root_id(Path::new("a")), "e40c292c");
        assert_eq!(System::hosts_tag(Path::new("a")), "# kubernix e40c292c");
    }

    #[test]
    fn hosts_entries_add_and_remove() -> Result<()> {
        let file = hosts_file(HOSTS)?;
        System::add_hosts_entries(file.path(), TAG_A, &entries())?;
        assert_eq!(
            read_to_string(file.path())?,
            format!(
                "{h}127.0.0.1 node-0 {t}\n127.0.0.1 node-1 {t}\n",
                h = HOSTS,
                t = TAG_A
            )
        );
        System::remove_hosts_entries(file.path(), TAG_A)?;
        assert_eq!(read_to_string(file.path())?, HOSTS);
        Ok(())
    }

    #[test]
    fn hosts_entries_keep_changes_while_running() -> Result<()> {
        let file = hosts_file(HOSTS)?;
        System::add_hosts_entries(file.path(), TAG_A, &entries())?;
        let mut content = read_to_string(file.path())?;
        content.push_str("10.0.0.1 added\n");
        fs::write(file.path(), content)?;
        System::remove_hosts_entries(file.path(), TAG_A)?;
        assert_eq!(
            read_to_string(file.path())?,
            format!("{}10.0.0.1 added\n", HOSTS)
        );
        Ok(())
    }

    #[test]
    fn hosts_entries_replace_stale() -> Result<()> {
        let file = hosts_file(&format!("{}127.0.0.1 node-5 {}\n", HOSTS, TAG_A))?;
        System::add_hosts_entries(file.path(), TAG_A, &entries())?;
        System::add_hosts_entries(file.path(), TAG_A, &entries())?;
        let content = read_to_string(file.path())?;
        assert!(!content.contains("node-5"));
        assert_eq!(content.matches("node-0").count(), 1);
        System::remove_hosts_entries(file.path(), TAG_A)?;
        assert_eq!(read_to_string(file.path())?, HOSTS);
        Ok(())
    }

    #[test]
    fn hosts_entries_multiple_roots() -> Result<()> {
        let file = hosts_file(HOSTS)?;
        System::add_hosts_entries(file.path(), TAG_A, &entries())?;
        System::add_hosts_entries(file.path(), TAG_B, &entries()[..1])?;
        let both = read_to_string(file.path())?;
        assert_eq!(both.matches(TAG_A).count(), 2);
        assert_eq!(both.matches(TAG_B).count(), 1);

        // The startup cleanup of another root keeps the entries
        System::remove_hosts_entries(file.path(), "# kubernix 0000000c")?;
        assert_eq!(read_to_string(file.path())?, both);

        System::remove_hosts_entries(file.path(), TAG_A)?;
        assert_eq!(
            read_to_string(file.path())?,
            format!("{}127.0.0.1 node-0 {}\n", HOSTS, TAG_B)
        );
        System::remove_hosts_entries(file.path(), TAG_B)?;
        assert_eq!(read_to_string(file.path())?, HOSTS);
        Ok(())
    }

    #[test]
    fn hosts_entries_replace_untagged_duplicates() -> Result<()> {
        let file = hosts_file(&format!(
            "{}127.0.0.1 node-0\n127.0.0.1 node-0 custom\n",
            HOSTS
        ))?;
        System::add_hosts_entries(file.path(), TAG_A, &entries())?;
        assert_eq!(
            read_to_string(file.path())?,
            format!(
                "{h}127.0.0.1 node-0 custom\n127.0.0.1 node-0 {t}\n127.0.0.1 node-1 {t}\n",
                h = HOSTS,
                t = TAG_A
            )
        );
        System::remove_hosts_entries(file.path(), TAG_A)?;
        assert_eq!(
            read_to_string(file.path())?,
            format!("{}127.0.0.1 node-0 custom\n", HOSTS)
        );
        Ok(())
    }

    #[test]
    fn hosts_entries_without_trailing_newline() -> Result<()> {
        let file = hosts_file("127.0.0.1 localhost")?;
        System::add_hosts_entries(file.path(), TAG_A, &entries()[..1])?;
        assert_eq!(
            read_to_string(file.path())?,
            format!("127.0.0.1 localhost\n127.0.0.1 node-0 {}\n", TAG_A)
        );
        Ok(())
    }

    #[test]
    fn hosts_entries_remove_untouched() -> Result<()> {
        let file = hosts_file("127.0.0.1 localhost")?;
        System::remove_hosts_entries(file.path(), TAG_A)?;
        assert_eq!(read_to_string(file.path())?, "127.0.0.1 localhost");
        assert!(System::remove_hosts_entries(Path::new("/should/not/exist"), TAG_A).is_err());
        Ok(())
    }

    #[test]
    fn find_executable_success() {
        assert!(System::find_executable(VALID_EXECUTABLE).is_ok());
    }

    #[test]
    fn find_executable_failure() {
        assert!(System::find_executable(INVALID_EXECUTABLE).is_err());
    }

    #[test]
    fn find_executable_in_path() -> Result<()> {
        let dir = tempfile::tempdir()?;
        fs::write(dir.path().join("tool"), "")?;
        let path = format!("/nonexistent:{}", dir.path().display());
        assert_eq!(
            System::find_executable_in("tool", Some(OsStr::new(&path)))?,
            dir.path().join("tool")
        );
        assert!(System::find_executable_in("tool", Some(OsStr::new("/nonexistent"))).is_err());
        assert!(System::find_executable_in("tool", None).is_err());
        Ok(())
    }

    #[test]
    fn find_shell_success() {
        temp_env::with_var("SHELL", Some(VALID_EXECUTABLE), || {
            assert!(System::shell().is_ok());
        });
    }
}
