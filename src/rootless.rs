//! Rootless mode prerequisites.
//!
//! Checks the host before re-executing inside rootlesskit, so that a
//! missing prerequisite fails early with an actionable message instead of
//! an obscure error of systemd-run, rootlesskit or a component later on.

use crate::{
    hoststate::{self, PROC_SYS},
    network::Network,
    process,
    system::System,
};
use anyhow::{Result, bail};
use log::{debug, error, warn};
use nix::{
    libc,
    sys::statfs::{CGROUP2_SUPER_MAGIC, statfs},
    unistd::{Uid, User, getuid},
};
use std::{
    env::var,
    ffi::CString,
    fs::{self, read_to_string},
    net::{IpAddr, Ipv4Addr},
    os::unix::{
        ffi::OsStrExt,
        fs::{MetadataExt, PermissionsExt},
    },
    path::{Path, PathBuf},
};

/// The documentation which explains how to fix the rootless prerequisites,
/// at the release of this version, so that moving it does not break the
/// link.
pub const TROUBLESHOOTING: &str = concat!(
    "https://github.com/saschagrunert/kubernix/blob/v",
    env!("CARGO_PKG_VERSION"),
    "/docs/troubleshooting.md"
);

/// The cgroup controllers which the components need, in the order of the
/// delegation.
pub const CGROUP_CONTROLLERS: [&str; 4] = ["memory", "cpu", "pids", "io"];

/// The environment variables of the proxy configuration.
const PROXY_VARS: [&str; 6] = [
    "http_proxy",
    "HTTP_PROXY",
    "https_proxy",
    "HTTPS_PROXY",
    "all_proxy",
    "ALL_PROXY",
];

/// The number of subordinate IDs below which images may fail to unpack.
const MIN_SUBIDS: u64 = 65536;

/// The `CAP_SETGID` capability bit.
const CAP_SETGID: u32 = 6;

/// The `CAP_SETUID` capability bit.
const CAP_SETUID: u32 = 7;

/// The outcome of the prerequisite checks.
#[derive(Debug, Default, PartialEq, Eq)]
struct Findings {
    /// Unmet prerequisites, which let the rootless session fail.
    errors: Vec<String>,
    /// Prerequisites which are likely unmet, but cannot be verified.
    warnings: Vec<String>,
}

impl Findings {
    fn error(&mut self, msg: impl Into<String>) {
        self.errors.push(msg.into());
    }

    fn warn(&mut self, msg: impl Into<String>) {
        self.warnings.push(msg.into());
    }
}

/// The rootless mode prerequisites of the host.
pub struct Rootless;

impl Rootless {
    /// Check the prerequisites of rootless mode before re-executing inside
    /// rootlesskit, which publishes the API server on `port`. Logs all
    /// unmet ones and fails if any of them would break the session.
    pub fn preflight(port: u16) -> Result<()> {
        let mut f = Findings::default();
        Self::check_systemd(&mut f);
        Self::check_subids(&mut f);
        for (helper, cap) in [("newuidmap", CAP_SETUID), ("newgidmap", CAP_SETGID)] {
            Self::check_helper(&mut f, helper, System::find_executable(helper).ok(), cap);
        }
        Self::check_sysctls(&mut f, |key| {
            hoststate::read_sysctl(Path::new(PROC_SYS), key).ok()
        });
        Self::check_cgroup2(&mut f);
        Self::check_port(&mut f, port);
        for name in PROXY_VARS {
            if let Ok(value) = var(name) {
                Self::check_proxy(&mut f, name, &value);
            }
        }

        for w in &f.warnings {
            warn!("{}", w);
        }
        for e in &f.errors {
            error!("{}", e);
        }
        if !f.errors.is_empty() {
            bail!(
                "Rootless mode prerequisites are not met, see {}",
                TROUBLESHOOTING
            )
        }
        Ok(())
    }

    /// Warn about every cgroup controller of `CGROUP_CONTROLLERS` which is
    /// not enabled for the children of the cgroup namespace root inside
    /// rootlesskit, for example because systemd does not delegate it to
    /// the user.
    pub fn warn_missing_controllers() {
        let subtree_control = Path::new("/sys/fs/cgroup/cgroup.subtree_control");
        let enabled = match read_to_string(subtree_control) {
            Ok(enabled) => enabled,
            Err(e) => {
                warn!(
                    "Unable to read the delegated cgroup controllers from '{}': {}",
                    subtree_control.display(),
                    e
                );
                return;
            }
        };
        for controller in Self::missing_controllers(&enabled) {
            warn!(
                "The cgroup controller '{}' is not delegated to the user, \
                 pods may fail to start or their {} limits may not apply, see {}",
                controller, controller, TROUBLESHOOTING
            );
        }
    }

    /// The controllers of `CGROUP_CONTROLLERS` missing in the content of a
    /// `cgroup.controllers` or `cgroup.subtree_control` file.
    fn missing_controllers(enabled: &str) -> Vec<&'static str> {
        let enabled: Vec<&str> = enabled.split_whitespace().collect();
        CGROUP_CONTROLLERS
            .into_iter()
            .filter(|c| !enabled.contains(c))
            .collect()
    }

    /// `systemd-run --user` needs the bus of the systemd user instance,
    /// which it finds via `$DBUS_SESSION_BUS_ADDRESS` or below
    /// `$XDG_RUNTIME_DIR`.
    fn check_systemd(f: &mut Findings) {
        if System::find_executable("systemd-run").is_err() {
            f.error(
                "Unable to find 'systemd-run' in $PATH, rootless mode needs \
                 systemd with a user session",
            );
            return;
        }
        let runtime_dir = var("XDG_RUNTIME_DIR").ok().filter(|x| !x.is_empty());
        let bus_address = var("DBUS_SESSION_BUS_ADDRESS")
            .ok()
            .filter(|x| !x.is_empty());
        if !Self::user_bus_reachable(
            bus_address.as_deref(),
            runtime_dir.as_deref().map(Path::new),
        ) {
            let uid = getuid();
            let user = Self::user_name(uid);
            f.error(format!(
                "No systemd user session found ({}), which 'systemd-run --user' needs. \
                 Log in with a regular session (for example via ssh or \
                 'machinectl shell {}@'), or enable lingering via \
                 'loginctl enable-linger {}' and set XDG_RUNTIME_DIR=/run/user/{}",
                match &runtime_dir {
                    Some(dir) => format!("neither '{dir}/bus' nor '{dir}/systemd/private' exist"),
                    None => "$XDG_RUNTIME_DIR and $DBUS_SESSION_BUS_ADDRESS are not set".into(),
                },
                user,
                user,
                uid,
            ));
        }
    }

    /// Returns true if systemd-run can connect to the user instance.
    fn user_bus_reachable(bus_address: Option<&str>, runtime_dir: Option<&Path>) -> bool {
        bus_address.is_some()
            || runtime_dir
                .is_some_and(|dir| dir.join("bus").exists() || dir.join("systemd/private").exists())
    }

    /// The name of the user, or the UID if unknown.
    fn user_name(uid: Uid) -> String {
        User::from_uid(uid)
            .ok()
            .flatten()
            .map_or_else(|| uid.to_string(), |u| u.name)
    }

    /// rootlesskit maps the subordinate IDs of the user into the user
    /// namespace. `getsubids` also covers the ones provided by NSS, the
    /// files are only authoritative without a `subid` NSS source.
    fn check_subids(f: &mut Findings) {
        let uid = getuid();
        let user = Self::user_name(uid);
        let getsubids = System::find_executable("getsubids").ok();
        let nss = read_to_string("/etc/nsswitch.conf")
            .map(|x| Self::nss_has_subid(&x))
            .unwrap_or(false);
        for (file, flag) in [("/etc/subuid", None), ("/etc/subgid", Some("-g"))] {
            let kind = if flag.is_some() { "GIDs" } else { "UIDs" };
            let count = match &getsubids {
                Some(getsubids) => Self::getsubids_count(getsubids, flag, &user),
                None => read_to_string(file)
                    .map(|x| Self::subid_file_count(&x, &user, uid))
                    .unwrap_or(0),
            };
            if count == 0 && getsubids.is_none() && nss {
                f.warn(format!(
                    "No subordinate {} for user '{}' found in '{}', \
                     assuming that NSS provides them as configured in /etc/nsswitch.conf",
                    kind, user, file
                ));
            } else if count == 0 {
                f.error(format!(
                    "No subordinate {kind} for user '{user}' found in '{file}', add them \
                     for example via 'sudo usermod --add-subuids 100000-165535 \
                     --add-subgids 100000-165535 {user}'",
                ));
            } else if count < MIN_SUBIDS {
                f.warn(format!(
                    "User '{}' has only {} subordinate {}, images with files of \
                     higher IDs fail to unpack, {} are recommended",
                    user, count, kind, MIN_SUBIDS
                ));
            }
        }
    }

    /// The amount of subordinate IDs of the user according to `getsubids`,
    /// zero if it fails.
    fn getsubids_count(getsubids: &Path, flag: Option<&str>, user: &str) -> u64 {
        let mut cmd = process::command(getsubids);
        cmd.args(flag).arg(user);
        match cmd.output() {
            Ok(output) if output.status.success() => {
                Self::getsubids_output_count(&String::from_utf8_lossy(&output.stdout))
            }
            Ok(output) => {
                debug!(
                    "getsubids exited with {}: {}",
                    output.status,
                    String::from_utf8_lossy(&output.stderr).trim()
                );
                0
            }
            Err(e) => {
                debug!("Unable to run getsubids: {}", e);
                0
            }
        }
    }

    /// The sum of the ranges in the `getsubids` output, which lists one
    /// range per line, for example `0: user 100000 65536`.
    fn getsubids_output_count(output: &str) -> u64 {
        output
            .lines()
            .filter_map(|line| line.split_whitespace().nth(3)?.parse::<u64>().ok())
            .sum()
    }

    /// The sum of the ranges of the user in a `/etc/subuid` or
    /// `/etc/subgid` file, whose lines are `user:start:count`, where the
    /// user may also be the numeric ID.
    fn subid_file_count(content: &str, user: &str, uid: Uid) -> u64 {
        let uid = uid.to_string();
        content
            .lines()
            .filter_map(|line| {
                let mut fields = line.trim().split(':');
                let name = fields.next()?;
                let count = fields.nth(1)?.parse::<u64>().ok()?;
                (name == user || name == uid).then_some(count)
            })
            .sum()
    }

    /// Returns true if the NSS configuration has a `subid` source.
    fn nss_has_subid(nsswitch: &str) -> bool {
        nsswitch.lines().any(|line| {
            line.trim_start()
                .strip_prefix("subid:")
                .is_some_and(|sources| !sources.trim().is_empty())
        })
    }

    /// rootlesskit writes the ID mappings via the `newuidmap` and
    /// `newgidmap` helpers of the host, which need to be setuid root or
    /// have the matching file capability.
    fn check_helper(f: &mut Findings, helper: &str, path: Option<PathBuf>, cap: u32) {
        let Some(path) = path else {
            f.error(format!(
                "Unable to find '{}' in $PATH, install it on the host (for example \
                 from the 'uidmap', 'shadow-utils' or 'shadow' package), Nix cannot \
                 provide setuid binaries",
                helper
            ));
            return;
        };
        let path = fs::canonicalize(&path).unwrap_or(path);
        let setuid_root =
            fs::metadata(&path).is_ok_and(|m| m.uid() == 0 && m.permissions().mode() & 0o4000 != 0);
        if !setuid_root && !Self::has_file_capability(&path, cap) {
            f.error(format!(
                "'{}' is neither setuid root nor has the {} file capability, \
                 install it from the package of the host distribution",
                path.display(),
                if cap == CAP_SETUID {
                    "CAP_SETUID"
                } else {
                    "CAP_SETGID"
                },
            ));
        }
    }

    /// Returns true if the permitted file capabilities of `path` contain
    /// `cap`.
    fn has_file_capability(path: &Path, cap: u32) -> bool {
        Self::file_capabilities(path).is_some_and(|data| Self::caps_permit(&data, cap))
    }

    /// The raw `security.capability` extended attribute of `path`.
    fn file_capabilities(path: &Path) -> Option<Vec<u8>> {
        let path = CString::new(path.as_os_str().as_bytes()).ok()?;
        let mut buf = [0u8; 64];
        // SAFETY: both names are NUL terminated and the size matches the
        // buffer.
        let len = unsafe {
            libc::getxattr(
                path.as_ptr(),
                c"security.capability".as_ptr(),
                buf.as_mut_ptr().cast(),
                buf.len(),
            )
        };
        let len = usize::try_from(len).ok()?;
        buf.get(..len).map(<[u8]>::to_vec)
    }

    /// Returns true if the `vfs_cap_data` permits `cap`, which is stored
    /// after the magic number as the first permitted capability word.
    fn caps_permit(data: &[u8], cap: u32) -> bool {
        data.get(4..8)
            .and_then(|x| x.try_into().ok())
            .map(u32::from_le_bytes)
            .is_some_and(|permitted| permitted & (1 << cap) != 0)
    }

    /// Check the sysctls which restrict unprivileged user namespaces,
    /// `read` returns the value of a sysctl if it exists.
    fn check_sysctls(f: &mut Findings, read: impl Fn(&str) -> Option<String>) {
        if read("kernel.unprivileged_userns_clone").as_deref() == Some("0") {
            f.error(
                "Unprivileged user namespaces are disabled, enable them via \
                 'sudo sysctl -w kernel.unprivileged_userns_clone=1'",
            );
        }
        if read("user.max_user_namespaces").as_deref() == Some("0") {
            f.error(
                "User namespaces are disabled (user.max_user_namespaces=0), enable \
                 them via 'sudo sysctl -w user.max_user_namespaces=28633'",
            );
        }
        if read("kernel.apparmor_restrict_unprivileged_userns").as_deref() == Some("1") {
            f.warn(
                "AppArmor restricts unprivileged user namespaces \
                 (kernel.apparmor_restrict_unprivileged_userns=1), which likely lets \
                 rootlesskit from the Nix store fail, unless an AppArmor profile \
                 allows it. Lift the restriction via \
                 'sudo sysctl -w kernel.apparmor_restrict_unprivileged_userns=0'",
            );
        }
    }

    /// The cgroup delegation of rootless mode requires the unified cgroup
    /// v2 hierarchy.
    fn check_cgroup2(f: &mut Findings) {
        let path = Path::new("/sys/fs/cgroup");
        match statfs(path) {
            Ok(fs) if fs.filesystem_type() == CGROUP2_SUPER_MAGIC => {}
            Ok(_) => f.error(
                "Rootless mode requires cgroup v2, but '/sys/fs/cgroup' is a cgroup v1 \
                 or hybrid hierarchy. Boot with 'systemd.unified_cgroup_hierarchy=1' \
                 on the kernel command line",
            ),
            Err(e) => f.warn(format!(
                "Unable to determine the cgroup version of '{}': {}",
                path.display(),
                e
            )),
        }
    }

    /// rootlesskit publishes the API server port on the loopback interface
    /// of the host, which fails after the startup if the port is in use.
    fn check_port(f: &mut Findings, port: u16) {
        let user = format!(
            "on {}, where rootless mode publishes the API server,",
            Ipv4Addr::LOCALHOST
        );
        match Network::port_in_use((Ipv4Addr::LOCALHOST, port).into(), &user) {
            Ok(None) => {}
            Ok(Some(message)) => f.error(message),
            Err(e) => f.warn(format!(
                "Unable to check if port {} on {} is free: {}",
                port,
                Ipv4Addr::LOCALHOST,
                e
            )),
        }
    }

    /// The rootless network namespace cannot reach the loopback interface
    /// of the host, so a proxy listening there is unusable.
    fn check_proxy(f: &mut Findings, name: &str, value: &str) {
        let host = Self::proxy_host(value);
        if Self::is_loopback(host) {
            f.warn(format!(
                "{} points to '{}' on the loopback interface of the host, which the \
                 rootless cluster cannot reach, so image pulls may fail. Let the proxy \
                 listen on another address of the host and use that one instead",
                name, host
            ));
        }
    }

    /// The host of a proxy URL like `http://user:pass@127.0.0.1:3128/`,
    /// where the scheme is optional.
    fn proxy_host(value: &str) -> &str {
        let rest = value.split_once("://").map_or(value, |(_, rest)| rest);
        let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
        let host = authority
            .rsplit_once('@')
            .map_or(authority, |(_, host)| host);
        match host.strip_prefix('[') {
            Some(v6) => v6.split(']').next().unwrap_or_default(),
            None => host.split(':').next().unwrap_or_default(),
        }
    }

    /// Returns true if the host refers to the loopback interface.
    fn is_loopback(host: &str) -> bool {
        let host = host.trim_end_matches('.').to_ascii_lowercase();
        host == "localhost"
            || host.ends_with(".localhost")
            || host.parse::<IpAddr>().is_ok_and(|ip| ip.is_loopback())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;

    #[test]
    fn user_bus_reachable() -> Result<()> {
        let dir = tempfile::tempdir()?;
        assert!(!Rootless::user_bus_reachable(None, None));
        assert!(!Rootless::user_bus_reachable(None, Some(dir.path())));
        assert!(Rootless::user_bus_reachable(
            Some("unix:path=/run/user/1000/bus"),
            None
        ));
        fs::create_dir(dir.path().join("systemd"))?;
        fs::write(dir.path().join("systemd/private"), "")?;
        assert!(Rootless::user_bus_reachable(None, Some(dir.path())));
        Ok(())
    }

    #[test]
    fn subid_counts() {
        let content = "other:100000:65536\nuser:165536:1000\n1000:300000:2000\n\nbroken\n";
        assert_eq!(
            Rootless::subid_file_count(content, "user", Uid::from_raw(1000)),
            3000
        );
        assert_eq!(
            Rootless::subid_file_count(content, "nobody", Uid::from_raw(2000)),
            0
        );
        assert_eq!(
            Rootless::getsubids_output_count("0: user 100000 65536\n1: user 200000 10\n"),
            65546
        );
        assert_eq!(Rootless::getsubids_output_count(""), 0);
    }

    #[test]
    fn nss_has_subid() {
        assert!(Rootless::nss_has_subid("passwd: files\nsubid: sss\n"));
        assert!(!Rootless::nss_has_subid("passwd: files\n#subid: sss\n"));
        assert!(!Rootless::nss_has_subid("subid:\n"));
    }

    #[test]
    fn caps_permit() {
        // cap_setuid=ep as written by setcap, revision 2
        let setuid = [
            1, 0, 0, 2, 0x80, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        ];
        assert!(Rootless::caps_permit(&setuid, CAP_SETUID));
        assert!(!Rootless::caps_permit(&setuid, CAP_SETGID));
        let setgid = [1, 0, 0, 2, 0x40, 0, 0, 0];
        assert!(Rootless::caps_permit(&setgid, CAP_SETGID));
        assert!(!Rootless::caps_permit(&[1, 0, 0, 2], CAP_SETUID));
    }

    #[test]
    fn check_helper_rejects_plain_binary() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let helper = dir.path().join("newuidmap");
        fs::write(&helper, "")?;
        fs::set_permissions(&helper, fs::Permissions::from_mode(0o755))?;
        let mut f = Findings::default();
        Rootless::check_helper(&mut f, "newuidmap", Some(helper), CAP_SETUID);
        Rootless::check_helper(&mut f, "newgidmap", None, CAP_SETGID);
        assert_eq!(f.errors.len(), 2, "{:?}", f);
        assert!(f.errors[0].contains("neither setuid root"), "{:?}", f);
        assert!(
            f.errors[1].contains("Unable to find 'newgidmap'"),
            "{:?}",
            f
        );
        Ok(())
    }

    #[test]
    fn check_sysctls() {
        let check = |values: &[(&str, &str)]| {
            let mut f = Findings::default();
            Rootless::check_sysctls(&mut f, |key| {
                values
                    .iter()
                    .find(|(k, _)| *k == key)
                    .map(|(_, v)| v.to_string())
            });
            f
        };
        assert_eq!(check(&[]), Findings::default());
        assert_eq!(
            check(&[
                ("kernel.unprivileged_userns_clone", "1"),
                ("user.max_user_namespaces", "1000"),
                ("kernel.apparmor_restrict_unprivileged_userns", "0"),
            ]),
            Findings::default()
        );
        let f = check(&[
            ("kernel.unprivileged_userns_clone", "0"),
            ("user.max_user_namespaces", "0"),
            ("kernel.apparmor_restrict_unprivileged_userns", "1"),
        ]);
        assert_eq!(f.errors.len(), 2);
        assert_eq!(f.warnings.len(), 1);
        assert!(f.warnings[0].contains("AppArmor"));
    }

    #[test]
    fn check_port() -> Result<()> {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))?;
        let port = listener.local_addr()?.port();
        let mut f = Findings::default();
        Rootless::check_port(&mut f, port);
        assert_eq!(f.errors.len(), 1);
        assert!(f.errors[0].contains("already in use"));
        drop(listener);
        Ok(())
    }

    #[test]
    fn proxy_host() {
        for (value, host) in [
            ("http://127.0.0.1:3128", "127.0.0.1"),
            ("http://user:p@ss@localhost:3128/", "localhost"),
            ("socks5://[::1]:1080", "::1"),
            ("proxy.example.com:8080", "proxy.example.com"),
            ("https://10.0.0.1", "10.0.0.1"),
            ("", ""),
        ] {
            assert_eq!(Rootless::proxy_host(value), host, "{}", value);
        }
    }

    #[test]
    fn check_proxy() {
        for (value, loopback) in [
            ("http://127.0.0.1:3128", true),
            ("http://127.1.2.3:3128", true),
            ("http://LOCALHOST:3128", true),
            ("http://proxy.localhost:3128", true),
            ("http://[::1]:3128", true),
            ("http://10.0.0.1:3128", false),
            ("http://proxy.example.com:3128", false),
            ("", false),
        ] {
            let mut f = Findings::default();
            Rootless::check_proxy(&mut f, "HTTPS_PROXY", value);
            assert_eq!(f.warnings.len(), usize::from(loopback), "{}", value);
            assert!(f.errors.is_empty());
        }
    }

    #[test]
    fn missing_controllers() {
        assert!(Rootless::missing_controllers("cpuset cpu io memory pids").is_empty());
        assert_eq!(
            Rootless::missing_controllers("cpu memory\n"),
            ["pids", "io"]
        );
        assert_eq!(Rootless::missing_controllers(""), CGROUP_CONTROLLERS);
    }
}
