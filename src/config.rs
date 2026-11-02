//! Configuration related structures
use crate::{network::Network, podman::Podman, system::System, write_file};
use anyhow::{Context, Result, bail};
use clap::{
    ArgMatches, CommandFactory, FromArgMatches, Parser, Subcommand as ClapSubcommand, ValueEnum,
    builder::{OsStringValueParser, PossibleValuesParser, TypedValueParser, styling},
    error::ErrorKind,
    parser::ValueSource,
};
use ipnetwork::Ipv4Network;
use log::LevelFilter;
use serde::{Deserialize, Serialize};
use std::{
    cmp::Ordering,
    collections::BTreeSet,
    fmt,
    fs::{self, DirBuilder, canonicalize, read_to_string},
    net::Ipv4Addr,
    os::unix::fs::{DirBuilderExt, MetadataExt},
    path::{Path, PathBuf, absolute},
    process::Command,
};

/// Network of slirp4netns, which rootlesskit uses in rootless mode.
pub(crate) const SLIRP4NETNS_CIDR: Ipv4Network =
    match Ipv4Network::new_checked(Ipv4Addr::new(10, 0, 2, 0), 24) {
        Some(n) => n,
        None => panic!("invalid slirp4netns network"),
    };

/// Output format for log messages.
#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq, Serialize, ValueEnum)]
#[serde(rename_all = "lowercase")]
pub(crate) enum LogFormat {
    /// Human-readable colored text
    #[default]
    Text,
    /// Machine-readable JSON, one object per line
    Json,
}

impl fmt::Display for LogFormat {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LogFormat::Text => write!(f, "text"),
            LogFormat::Json => write!(f, "json"),
        }
    }
}

/// Container Runtime Interface (CRI) implementation to use.
#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq, Serialize, ValueEnum)]
#[serde(rename_all = "lowercase")]
pub(crate) enum CriRuntime {
    /// CRI-O container runtime
    #[default]
    Crio,
    /// containerd container runtime
    Containerd,
}

impl fmt::Display for CriRuntime {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CriRuntime::Crio => write!(f, "crio"),
            CriRuntime::Containerd => write!(f, "containerd"),
        }
    }
}

/// OCI runtime used as the default handler of the CRI runtime.
///
/// Both runtimes are always registered as CRI handlers, so pods can select
/// the other one via a `RuntimeClass`.
#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq, Serialize, ValueEnum)]
#[serde(rename_all = "lowercase")]
pub(crate) enum OciRuntime {
    /// crun OCI runtime
    #[default]
    Crun,
    /// runc OCI runtime
    Runc,
}

impl OciRuntime {
    /// All supported OCI runtimes, registered as CRI runtime handlers.
    pub(crate) const ALL: [OciRuntime; 2] = [OciRuntime::Crun, OciRuntime::Runc];

    /// The runtime handler and executable name.
    pub(crate) fn name(self) -> &'static str {
        match self {
            OciRuntime::Crun => "crun",
            OciRuntime::Runc => "runc",
        }
    }
}

impl fmt::Display for OciRuntime {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.name())
    }
}

/// Cluster addon deployed after the bootstrap.
#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize, ValueEnum)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Addon {
    /// CoreDNS for the cluster DNS
    Coredns,
    /// Disable all addons, only valid on its own
    None,
}

impl fmt::Display for Addon {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Addon::Coredns => write!(f, "coredns"),
            Addon::None => write!(f, "none"),
        }
    }
}

#[derive(Clone, Parser, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
#[command(
    about = "Kubernetes development cluster bootstrapping with Nix packages",
    after_help = "More info at: https://github.com/saschagrunert/kubernix",
    long_about = None,
    author = "Sascha Grunert <mail@saschagrunert.de>",
    version,
    styles = styling::Styles::styled()
        .header(styling::AnsiColor::Green.on_default().bold())
        .usage(styling::AnsiColor::Green.on_default().bold())
        .literal(styling::AnsiColor::Cyan.on_default().bold())
        .placeholder(styling::AnsiColor::Cyan.on_default())
)]
/// Kubernix runtime configuration parsed from CLI arguments and config files.
pub struct Config {
    /// Format version of the run directory and this configuration, see
    /// `Config::VERSION`. Missing in run directories created before the
    /// versioning, which have version 0.
    #[serde(default)]
    #[arg(skip = Config::VERSION)]
    version: u32,

    #[command(subcommand)]
    /// All available subcommands
    subcommand: Option<SubCommand>,

    #[arg(
        default_value = "kubernix-run",
        env = "KUBERNIX_ROOT",
        global = true,
        long = "root",
        short = 'r',
        value_name = "PATH"
    )]
    /// Path where all the runtime data is stored
    root: PathBuf,

    #[arg(
        default_value = "info",
        env = "KUBERNIX_LOG_LEVEL",
        ignore_case = true,
        long = "log-level",
        short = 'l',
        value_name = "LEVEL",
        value_parser = PossibleValuesParser::new(["off", "error", "warn", "info", "debug", "trace"])
            .try_map(|s| s.parse::<LevelFilter>()),
    )]
    /// Logging verbosity
    log_level: LevelFilter,

    #[arg(
        default_value = "text",
        env = "KUBERNIX_LOG_FORMAT",
        long = "log-format",
        short_alias = 'f',
        value_name = "FORMAT"
    )]
    /// The output format for log messages
    log_format: LogFormat,

    #[arg(
        default_value = "10.10.0.0/16",
        env = "KUBERNIX_CIDR",
        long = "cidr",
        short = 'c',
        value_name = "CIDR",
        value_parser = Config::parse_cidr,
    )]
    /// The CIDR used for the cluster, host bits are ignored
    cidr: Ipv4Network,

    #[arg(
        env = "KUBERNIX_OVERLAY",
        long = "overlay",
        short = 'o',
        value_name = "PATH",
        value_parser = OsStringValueParser::new().map(PathBuf::from),
    )]
    /// The Nix package overlay to be used, an empty value disables it
    overlay: Option<PathBuf>,

    #[arg(
        env = "KUBERNIX_PACKAGES",
        long = "packages",
        short = 'p',
        value_delimiter = ',',
        value_name = "PACKAGE"
    )]
    /// Additional Nix packages to be added to the environment, comma separated
    /// or repeated, an empty value adds none
    packages: Vec<String>,

    #[arg(
        env = "KUBERNIX_SHELL",
        long = "shell",
        short = 's',
        value_name = "SHELL"
    )]
    /// The shell executable to be used, defaults to $SHELL, fallback is `sh`
    shell: Option<String>,

    #[arg(
        default_value = "1",
        env = "KUBERNIX_NODES",
        long = "nodes",
        short = 'n',
        value_name = "NODES",
        value_parser = clap::value_parser!(u8).range(1..=64),
    )]
    /// The number of nodes to be registered (1 to 64)
    nodes: u8,

    #[arg(
        env = "KUBERNIX_CONTAINER_RUNTIME",
        long = "container-runtime",
        default_value = Podman::EXECUTABLE,
        short_alias = 'u',
        value_name = "EXECUTABLE",
    )]
    /// Container runtime for the node containers, only used in multinode root mode
    container_runtime: String,

    #[arg(env = "KUBERNIX_NO_SHELL", long = "no-shell", short_alias = 'e')]
    /// Do not spawn an interactive shell after bootstrap
    no_shell: bool,

    #[arg(
        env = "KUBERNIX_DOCKERFILE",
        long = "dockerfile",
        short = 'd',
        value_name = "PATH",
        value_parser = OsStringValueParser::new().map(PathBuf::from),
    )]
    /// Custom Dockerfile for the node container image, only used in multinode
    /// root mode, an empty value disables it
    dockerfile: Option<PathBuf>,

    #[serde(default)]
    #[arg(
        default_value = "crio",
        env = "KUBERNIX_CRI_RUNTIME",
        long = "cri-runtime",
        value_name = "RUNTIME"
    )]
    /// The CRI runtime to use
    cri_runtime: CriRuntime,

    #[serde(default)]
    #[arg(
        default_value = "crun",
        env = "KUBERNIX_OCI_RUNTIME",
        long = "oci-runtime",
        value_name = "RUNTIME"
    )]
    /// The default OCI runtime, both are registered as runtime handlers
    oci_runtime: OciRuntime,

    /// Whether the run directory belongs to a rootless cluster, unset for
    /// configuration files written by older versions
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[arg(skip)]
    rootless: Option<bool>,

    /// The ids of the arguments explicitly set on the command line or via
    /// the environment
    #[serde(skip)]
    #[arg(skip)]
    explicit: BTreeSet<String>,

    #[arg(
        default_value = "coredns",
        env = "KUBERNIX_ADDONS",
        long = "addons",
        short = 'a',
        value_delimiter = ',',
        value_name = "ADDON"
    )]
    /// Cluster addons to deploy, comma separated or repeated, `none` disables
    /// all
    addons: Vec<Addon>,
}

impl Config {
    /// The active subcommand, if any (e.g. `shell`).
    pub fn subcommand(&self) -> &Option<SubCommand> {
        &self.subcommand
    }

    /// Directory where all runtime data is stored.
    pub(crate) fn root(&self) -> &Path {
        &self.root
    }

    /// Configured log verbosity level.
    pub(crate) fn log_level(&self) -> LevelFilter {
        self.log_level
    }

    /// Output format for log messages (text or JSON).
    pub(crate) fn log_format(&self) -> LogFormat {
        self.log_format
    }

    /// Parent CIDR, split into the cluster CIDR, which contains the pod
    /// CIDRs of all nodes, and the service CIDR.
    pub(crate) fn cidr(&self) -> Ipv4Network {
        self.cidr
    }

    /// Custom Nix package overlay path, if provided via `--overlay`.
    pub(crate) fn overlay(&self) -> Option<&Path> {
        self.overlay.as_deref()
    }

    /// Additional Nix packages to include in the runtime environment.
    pub(crate) fn packages(&self) -> &[String] {
        &self.packages
    }

    /// Interactive shell executable (defaults to `$SHELL`, fallback `sh`).
    pub(crate) fn shell(&self) -> Option<&str> {
        self.shell.as_deref()
    }

    /// Number of cluster nodes to register.
    pub(crate) fn nodes(&self) -> u8 {
        self.nodes
    }

    /// Container runtime executable used for multi-node setups.
    pub(crate) fn container_runtime(&self) -> &str {
        &self.container_runtime
    }

    /// Whether to skip spawning an interactive shell after bootstrap.
    pub(crate) fn no_shell(&self) -> bool {
        self.no_shell
    }

    /// Custom Dockerfile path for multi-node container image builds.
    pub(crate) fn dockerfile(&self) -> Option<&Path> {
        self.dockerfile.as_deref()
    }

    /// CRI runtime implementation (CRI-O or containerd).
    pub(crate) fn cri_runtime(&self) -> CriRuntime {
        self.cri_runtime
    }

    /// Default OCI runtime handler (crun or runc).
    pub(crate) fn oci_runtime(&self) -> OciRuntime {
        self.oci_runtime
    }

    /// Cluster addons to deploy after bootstrap (e.g. `coredns`).
    pub(crate) fn addons(&self) -> &[Addon] {
        &self.addons
    }

    /// Whether the cluster is running in rootless (user namespace) mode.
    pub(crate) fn is_rootless(&self) -> bool {
        self.rootless.unwrap_or_default()
    }

    /// Set rootless mode (auto-detected from UID at startup).
    pub(crate) fn set_rootless(&mut self, val: bool) {
        self.rootless = Some(val);
    }
}

/// Possible subcommands
#[derive(Clone, ClapSubcommand, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub enum SubCommand {
    /// Spawn an additional shell session, or run a command in it
    #[command(name = "shell")]
    Shell {
        #[arg(last = true, value_name = "COMMAND")]
        /// The command to run instead of an interactive shell, after `--`
        command: Vec<String>,
    },

    /// Stop the running cluster
    #[command(name = "stop")]
    Stop {
        #[arg(
            default_value = "180",
            long = "timeout",
            short = 't',
            value_name = "SECONDS"
        )]
        /// The time to wait for the cleanup to finish
        timeout: u64,
    },

    /// Show the status of the cluster
    #[command(name = "status")]
    Status {
        #[arg(default_value = "text", long = "output", value_name = "FORMAT")]
        /// The output format
        output: OutputFormat,
    },
}

/// Output format of `kubernix status`.
#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq, Serialize, ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum OutputFormat {
    /// Human-readable text
    #[default]
    Text,
    /// Machine-readable JSON
    Json,
}

impl Config {
    /// Parse configuration from CLI arguments.
    pub fn from_args() -> Self {
        let mut config =
            Self::from_matches(&Self::command().get_matches()).unwrap_or_else(|e| e.exit());
        if config.shell.is_none() {
            config.shell = System::shell().ok();
        }
        config
    }

    /// Remove the environment variables of the options from the command,
    /// which runs the next stage of kubernix with all options passed as
    /// arguments.
    pub(crate) fn remove_option_env(cmd: &mut Command) {
        for arg in Self::command().get_arguments() {
            if let Some(name) = arg.get_env() {
                cmd.env_remove(name);
            }
        }
    }

    /// Create the configuration from parsed arguments and remember which
    /// of them have been set explicitly.
    fn from_matches(matches: &ArgMatches) -> Result<Self, clap::Error> {
        let mut config = Self::from_arg_matches(matches)?;
        config.explicit = Self::command()
            .get_arguments()
            .map(|a| a.get_id().as_str())
            .filter(|id| {
                matches!(
                    matches.value_source(id),
                    Some(ValueSource::CommandLine | ValueSource::EnvVariable)
                )
            })
            .map(String::from)
            .collect();
        // An empty value unsets the option, which allows to override a
        // stored value.
        let set = |p: &PathBuf| !p.as_os_str().is_empty();
        config.overlay = config.overlay.filter(set).map(Self::absolute);
        config.dockerfile = config.dockerfile.filter(set).map(Self::absolute);
        config.packages.retain(|p| !p.is_empty());

        // `KUBERNIX_NO_SHELL=false` is the same as not setting it.
        if !config.no_shell {
            config.explicit.remove("no_shell");
        }
        // `--no-shell` and `--shell` exclude each other, where the command
        // line takes precedence over the environment.
        if config.no_shell && config.shell.is_some() {
            let source = |id| matches.value_source(id);
            match source("no_shell").cmp(&source("shell")) {
                Ordering::Greater => {
                    config.shell = None;
                    config.explicit.remove("shell");
                }
                Ordering::Less => {
                    config.no_shell = false;
                    config.explicit.remove("no_shell");
                }
                Ordering::Equal => {
                    return Err(Self::command().error(
                        ErrorKind::ArgumentConflict,
                        "the argument '--no-shell' cannot be used with '--shell <SHELL>'",
                    ));
                }
            }
        }

        if config.addons.contains(&Addon::None) {
            if config.addons.len() > 1 {
                return Err(Self::command().error(
                    ErrorKind::ArgumentConflict,
                    "the addon 'none' cannot be combined with other addons",
                ));
            }
            config.addons.clear();
        }
        Ok(config)
    }

    /// Parse a CIDR and remove its host bits, so that `10.10.0.1/16` equals
    /// `10.10.0.0/16`.
    fn parse_cidr(s: &str) -> Result<Ipv4Network, String> {
        s.parse()
            .map(Self::normalize_cidr)
            .map_err(|e| format!("{}", e))
    }

    /// Remove the host bits of the CIDR.
    fn normalize_cidr(cidr: Ipv4Network) -> Ipv4Network {
        Ipv4Network::new(cidr.network(), cidr.prefix()).unwrap_or(cidr)
    }

    /// Make a path absolute, so that it does not depend on the working
    /// directory once stored.
    fn absolute(path: PathBuf) -> PathBuf {
        absolute(&path).unwrap_or(path)
    }

    /// Returns true if the argument has been set explicitly.
    fn is_explicit(&self, id: &str) -> bool {
        self.explicit.contains(id)
    }
}

/// Compatibility of a stored run directory version with the current one.
#[derive(Debug, PartialEq, Eq)]
enum Compat {
    /// Same version, usable as is
    Current,
    /// Older version, which can be migrated in place
    Migrate,
    /// Older version, which cannot be migrated
    Outdated,
    /// Created by a newer kubernix
    Newer,
}

impl Compat {
    /// The compatibility of `version` with the `current` one, where all
    /// versions starting at `oldest` can be migrated.
    fn of(version: u32, current: u32, oldest: u32) -> Self {
        if version > current {
            Compat::Newer
        } else if version == current {
            Compat::Current
        } else if version >= oldest {
            Compat::Migrate
        } else {
            Compat::Outdated
        }
    }
}

/// The part of the configuration file which is read before the rest, so
/// that the version check works for all file formats.
#[derive(Deserialize)]
struct Version {
    #[serde(default)]
    version: u32,
    #[serde(default)]
    rootless: Option<bool>,
}

/// The attributes of a directory which decide whether root can trust it.
#[derive(Clone, Copy, Debug)]
struct DirAttrs {
    /// Whether it is a directory, symbolic links are not followed
    dir: bool,
    uid: u32,
    gid: u32,
    mode: u32,
}

impl DirAttrs {
    /// Read the attributes of `path`.
    fn read(path: &Path) -> Result<Self> {
        let m = fs::symlink_metadata(path)
            .with_context(|| format!("Unable to access '{}'", path.display()))?;
        Ok(Self {
            dir: m.is_dir(),
            uid: m.uid(),
            gid: m.gid(),
            mode: m.mode(),
        })
    }

    /// Returns true if the group `gid` is the private group of the user
    /// `uid`, like the ones of distributions with user private groups
    /// (umask 0002). Its write permission does not allow anybody else to
    /// write then. The group has to be the primary group of the user, have
    /// the same name, no other members listed and must not be the primary
    /// group of another user. The latter is only checked in `/etc/passwd`,
    /// since NSS sources like LDAP usually cannot enumerate their users.
    fn private_group(uid: u32, gid: u32) -> bool {
        use ::nix::unistd::{Gid, Group, Uid, User};
        match (
            User::from_uid(Uid::from_raw(uid)),
            Group::from_gid(Gid::from_raw(gid)),
        ) {
            (Ok(Some(user)), Ok(Some(group))) => {
                user.gid == group.gid
                    && user.name == group.name
                    && group.mem.iter().all(|m| *m == user.name)
                    && fs::read_to_string("/etc/passwd")
                        .is_ok_and(|passwd| !Self::other_primary_member(&passwd, uid, gid))
            }
            _ => false,
        }
    }

    /// Returns true if a user other than `uid` has the primary group `gid`
    /// in the `passwd` content, whose lines are
    /// `name:password:uid:gid:gecos:home:shell`.
    fn other_primary_member(passwd: &str, uid: u32, gid: u32) -> bool {
        passwd.lines().any(|line| {
            let mut fields = line.split(':').skip(2);
            let (Some(Ok(u)), Some(Ok(g))) = (
                fields.next().map(str::parse::<u32>),
                fields.next().map(str::parse::<u32>),
            ) else {
                return false;
            };
            u != uid && g == gid
        })
    }
}

impl Config {
    /// The configuration file in the run root.
    pub(crate) const FILENAME: &'static str = "kubernix.toml";

    /// Format version of new run directories and configuration files.
    ///
    /// - 0: Run directories created before the versioning
    /// - 1: Adds the version, otherwise identical to 0
    /// - 2: New CIDR layout and PKI (front proxy CA, `kubelet-<node>` files),
    ///   not migratable
    ///
    /// Changes of the PKI need no new version as long as they keep the
    /// cluster CA and the ServiceAccount key, since missing or outdated
    /// certificates get regenerated.
    ///
    /// Bump it if an existing run directory or configuration file cannot be
    /// used as is anymore, and either migrate the older versions in
    /// `migrate` or refuse them by raising `OLDEST_VERSION`.
    pub(crate) const VERSION: u32 = 2;

    /// Oldest version which gets migrated in place to `VERSION`. Older
    /// run directories are refused and have to be removed.
    const OLDEST_VERSION: u32 = 2;

    /// Check the format version of a stored configuration file of a root
    /// or rootless (`rootless`) run directory. Returns true if the run
    /// directory has to be migrated.
    fn check_version(&self, version: u32, rootless: bool) -> Result<bool> {
        let supported = if Self::OLDEST_VERSION == Self::VERSION {
            format!("format version {}", Self::VERSION)
        } else {
            format!(
                "format versions {} to {}",
                Self::OLDEST_VERSION,
                Self::VERSION
            )
        };
        match Compat::of(version, Self::VERSION, Self::OLDEST_VERSION) {
            Compat::Current => Ok(false),
            Compat::Migrate => Ok(true),
            Compat::Outdated => bail!(
                "The run directory '{}' has format version {}, which cannot be migrated \
                 since this kubernix only supports {}. Remove the run directory to start \
                 a new cluster: {}",
                self.root().display(),
                version,
                supported,
                Self::remove_command(self.root(), rootless),
            ),
            Compat::Newer => bail!(
                "The run directory '{}' has format version {} of a newer kubernix, while \
                 this kubernix only supports {}. Upgrade kubernix, or remove the run \
                 directory to start a new cluster: {}",
                self.root().display(),
                version,
                supported,
                Self::remove_command(self.root(), rootless),
            ),
        }
    }

    /// The command which removes the run directory `root`. Rootless clusters
    /// leave files of subordinate users behind, so their removal requires
    /// a user namespace which maps them.
    pub(crate) fn remove_command(root: &Path, rootless: bool) -> String {
        let root = crate::shell::quote(&root.display().to_string());
        if rootless {
            format!("unshare --map-auto --map-root-user rm -rf {}", root)
        } else {
            format!("sudo rm -rf {}", root)
        }
    }

    /// Migrate the stored configuration of an older run directory to the
    /// current version.
    fn migrate(mut stored: Config) -> Config {
        // No version is migratable at the moment
        stored.version = Self::VERSION;
        stored
    }

    /// Make the configs root path absolute, and ensure that it can be
    /// trusted in root mode.
    pub(crate) fn canonicalize_root(&mut self) -> Result<()> {
        self.create_root_dir()?;
        self.root = canonicalize(self.root()).with_context(|| {
            format!(
                "Unable to canonicalize the run directory '{}'",
                self.root().display()
            )
        })?;
        if ::nix::unistd::Uid::effective().is_root() && !self.is_rootless() {
            let mut owners = vec![0];
            owners.extend(
                std::env::var("SUDO_UID")
                    .ok()
                    .and_then(|uid| uid.parse::<u32>().ok()),
            );
            Self::check_trusted_root(
                self.root(),
                &owners,
                DirAttrs::read,
                DirAttrs::private_group,
            )?;
        }
        Ok(())
    }

    /// The explanation for a directory owned by `uid`, which is not one of
    /// the trusted `owners`. Without `SUDO_UID`, only root is trusted, for
    /// example as root via `su`, `doas`, SSH or inside a container with a
    /// mounted directory of a user.
    fn untrusted_owner_hint(owners: &[u32], uid: u32) -> String {
        if owners.contains(&uid) || owners.len() > 1 {
            return String::new();
        }
        format!(
            ". Without sudo (SUDO_UID is not set, for example via su, doas, a root login or \
             inside a container), only directories of root are trusted. Run kubernix via \
             sudo as uid {}, which trusts the invoking user, or use a run directory of \
             root, for example --root /var/lib/kubernix",
            uid
        )
    }

    /// Ensure that no other users than the `owners` can modify the run
    /// root. Root mode writes keys into it, runs its configuration (for
    /// example the Nix overlay) and hands out the admin kubeconfig, so
    /// another user who is able to plant files or symbolic links in it
    /// would gain root.
    ///
    /// The run root and all its parents have to belong to one of the
    /// `owners`, which are root and the user running kubernix via sudo,
    /// since the owner of a parent can replace the run root. None of them
    /// may be writable by others, or by their group unless it is the
    /// private group of their owner (see `private_group`). Parents with the
    /// sticky bit set (like `/tmp`) are exempt from the latter, since
    /// nobody else than their owner can replace the run root in them.
    ///
    /// The attributes of the directories come from `attrs`, and
    /// `private_group` checks if a group (gid) is the private one of a user
    /// (uid).
    fn check_trusted_root(
        root: &Path,
        owners: &[u32],
        attrs: impl Fn(&Path) -> Result<DirAttrs>,
        private_group: impl Fn(u32, u32) -> bool,
    ) -> Result<()> {
        let writable = |a: &DirAttrs| {
            a.mode & 0o002 != 0 || (a.mode & 0o020 != 0 && !private_group(a.uid, a.gid))
        };
        let owners_list = owners
            .iter()
            .map(u32::to_string)
            .collect::<Vec<_>>()
            .join(" or ");

        let a = attrs(root)?;
        if !a.dir || !owners.contains(&a.uid) || writable(&a) {
            bail!(
                "The run directory '{}' has to be a directory owned by uid {}, which others \
                 cannot write (owner {}, mode {:o}), use a different --root or fix it, for \
                 example via 'chown root:root' and 'chmod go-w'{}",
                root.display(),
                owners_list,
                a.uid,
                a.mode & 0o7777,
                Self::untrusted_owner_hint(owners, a.uid),
            )
        }
        for parent in root.ancestors().skip(1) {
            let a = attrs(parent)?;
            if !owners.contains(&a.uid) || (writable(&a) && a.mode & 0o1000 == 0) {
                bail!(
                    "The parent directory '{}' of the run directory '{}' has to be owned by uid \
                     {} and must not be writable by others (owner {}, mode {:o}), since \
                     otherwise they can replace the run directory, use a different --root{}",
                    parent.display(),
                    root.display(),
                    owners_list,
                    a.uid,
                    a.mode & 0o7777,
                    Self::untrusted_owner_hint(owners, a.uid),
                )
            }
        }
        Ok(())
    }

    /// Write the current configuration to the internal set root path
    pub(crate) fn to_file(&self) -> Result<()> {
        self.create_root_dir()?;
        write_file(
            &self.root().join(Self::FILENAME),
            toml::to_string(&self)?.as_bytes(),
        )
        .context("Unable to write configuration to file")?;
        Ok(())
    }

    /// The path of the configuration file in the root.
    pub(crate) fn file(&self) -> PathBuf {
        self.root().join(Self::FILENAME)
    }

    /// Returns true if the configuration file exists in the root path.
    pub(crate) fn file_exists(&self) -> bool {
        self.file().exists()
    }

    /// Read the stored configuration from the root path and merge it into
    /// the current one, after migrating it from an older version if needed.
    /// The file stays untouched, a migrated configuration gets returned for
    /// `write_migrated`.
    pub(crate) fn load_file(&mut self) -> Result<Option<Config>> {
        let file = self.file();
        let content = read_to_string(&file).with_context(|| {
            format!(
                "Unable to read expected configuration file '{}'",
                file.display(),
            )
        })?;
        let load_err = || format!("Unable to load config file '{}'", file.display());
        let version = toml::from_str::<Version>(&content).with_context(load_err)?;
        let rootless = version.rootless.unwrap_or(self.is_rootless());
        let migrate = self.check_version(version.version, rootless)?;
        let stored: Config = toml::from_str(&content).with_context(load_err)?;
        if migrate {
            let stored = Self::migrate(stored);
            self.merge(stored.clone())?;
            Ok(Some(stored))
        } else {
            self.merge(stored)?;
            Ok(None)
        }
    }

    /// Write the migrated configuration returned by `load_file`.
    pub(crate) fn write_migrated(&self, migrated: &Config) -> Result<()> {
        let file = self.file();
        write_file(&file, toml::to_string(migrated)?.as_bytes())
            .with_context(|| format!("Unable to write migrated config file '{}'", file.display()))
    }

    /// Check the mode of the existing run directory like `merge`, but
    /// before the run directory is trusted, so that root gets refused for
    /// a rootless run directory because of its mode instead of its
    /// ownership. Only the `rootless` flag of the file gets used, a file
    /// which cannot be read or parsed gets refused later by `load_file`.
    pub(crate) fn check_stored_mode(&mut self) -> Result<()> {
        use std::{io::Read, os::unix::fs::OpenOptionsExt};
        // Neither follow symbolic links nor block on other file types
        let content = fs::OpenOptions::new()
            .read(true)
            .custom_flags(::nix::libc::O_NOFOLLOW | ::nix::libc::O_NONBLOCK)
            .open(self.file())
            .and_then(|file| {
                if !file.metadata()?.is_file() {
                    return Err(std::io::ErrorKind::InvalidInput.into());
                }
                let mut content = String::new();
                file.take(1 << 20).read_to_string(&mut content)?;
                Ok(content)
            });
        match content.map(|c| toml::from_str::<Version>(&c)) {
            Ok(Ok(Version {
                rootless: Some(rootless),
                ..
            })) => self.check_mode(rootless),
            _ => Ok(()),
        }
    }

    /// Check that a run directory of a root or rootless (`rootless`)
    /// cluster can be used in the current mode. Only `stop` and `status`
    /// can be used by other users on a root cluster, which switches to root
    /// mode.
    fn check_mode(&mut self, rootless: bool) -> Result<()> {
        if rootless == self.is_rootless() {
            return Ok(());
        }
        let mode = |rootless| if rootless { "rootless" } else { "root" };
        match &self.subcommand {
            // Reading the state of a root cluster works for other users
            // as well, and stopping it fails with the sudo command if
            // it runs.
            Some(SubCommand::Stop { .. } | SubCommand::Status { .. }) if !rootless => {
                self.rootless = Some(false);
                Ok(())
            }
            Some(SubCommand::Shell { .. }) if !rootless => bail!(
                "The kubernix cluster in '{}' runs as root, use '{}' to access it",
                self.root().display(),
                Self::sudo_command_line(),
            ),
            _ => bail!(
                "The run directory '{}' belongs to a {} cluster and cannot be used in {} mode, \
                 use a different --root or remove the run directory",
                self.root().display(),
                mode(rootless),
                mode(self.is_rootless()),
            ),
        }
    }

    /// Merge the stored configuration of an existing run directory.
    ///
    /// Settings which shape the cluster have to match the stored ones,
    /// since the existing cluster state depends on them. The other settings
    /// are taken from the stored configuration, unless they have been set
    /// explicitly. The root and the rootless mode are never taken from the
    /// stored configuration, except that `stop` and `status` can be used by
    /// other users on a root cluster.
    fn merge(&mut self, mut stored: Config) -> Result<()> {
        if let Some(rootless) = stored.rootless {
            self.check_mode(rootless)?;
        }

        stored.overlay = stored.overlay.map(Self::absolute);
        stored.dockerfile = stored.dockerfile.map(Self::absolute);
        stored.cidr = Self::normalize_cidr(stored.cidr);

        // The subcommands use the existing cluster as it is, so exported
        // cluster settings do not matter for them.
        let bootstrap = self.subcommand.is_none();
        macro_rules! cluster {
            ($($field:ident),*) => {$(
                if bootstrap
                    && self.is_explicit(stringify!($field))
                    && self.$field != stored.$field
                {
                    bail!(
                        "The {} setting {} differs from the existing cluster ({}) in '{}', \
                         remove the run directory to change it",
                        stringify!($field).replace('_', "-"),
                        serde_json::to_string(&self.$field).unwrap_or_default(),
                        serde_json::to_string(&stored.$field).unwrap_or_default(),
                        self.root().display(),
                    )
                }
                self.$field = stored.$field;
            )*};
        }
        cluster!(nodes, cidr, cri_runtime, oci_runtime, container_runtime);

        macro_rules! overridable {
            ($($field:ident),*) => {$(
                if !self.is_explicit(stringify!($field)) {
                    self.$field = stored.$field;
                }
            )*};
        }
        // The Nix environment gets regenerated if its inputs change
        overridable!(log_level, log_format, addons, dockerfile, overlay, packages);

        // The shell options conflict with each other, so they are only
        // taken together.
        if !self.is_explicit("shell") && !self.is_explicit("no_shell") {
            self.shell = stored.shell.or(self.shell.take());
            self.no_shell = stored.no_shell;
        }
        Ok(())
    }

    /// The command which runs the current kubernix command line as root.
    pub(crate) fn sudo_command_line() -> String {
        Self::sudo_command(
            &std::env::current_exe().unwrap_or_else(|_| "kubernix".into()),
            &std::env::args().skip(1).collect::<Vec<_>>(),
        )
    }

    /// The command which runs kubernix with the provided arguments as
    /// root, keeping the environment and the `PATH` to find Nix.
    fn sudo_command(exe: &Path, args: &[String]) -> String {
        let args: Vec<String> = std::iter::once(exe.display().to_string())
            .chain(args.iter().cloned())
            .map(|a| crate::shell::quote(&a))
            .collect();
        format!("sudo -E env \"PATH=$PATH\" {}", args.join(" "))
    }

    /// Validate the settings before bootstrapping a cluster with them.
    pub(crate) fn validate(&self) -> Result<()> {
        if self.is_rootless() && self.cidr().overlaps(SLIRP4NETNS_CIDR) {
            bail!(
                "The CIDR '{}' overlaps with the slirp4netns network '{}', \
                 which provides the network connectivity in rootless mode",
                self.cidr(),
                SLIRP4NETNS_CIDR,
            )
        }
        Network::check_layout(self.cidr(), self.nodes())?;
        self.check_packages()?;
        for (name, option, path) in [
            ("overlay", "overlay", self.overlay()),
            ("Dockerfile", "dockerfile", self.dockerfile()),
        ] {
            if let Some(path) = path
                && !path.is_file()
            {
                bail!(
                    "The {} '{}' is not a file, use '--{}=' to not use one",
                    name,
                    path.display(),
                    option,
                )
            }
        }
        Ok(())
    }

    /// Check that the additional packages are Nix attribute paths, like
    /// `jq` or `python3Packages.pyyaml`, since they become part of a Nix
    /// expression.
    pub(crate) fn check_packages(&self) -> Result<()> {
        const KEYWORDS: [&str; 10] = [
            "assert", "else", "if", "in", "inherit", "let", "or", "rec", "then", "with",
        ];
        let identifier = |attr: &str| {
            let mut chars = attr.chars();
            chars
                .next()
                .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
                && chars.all(|c| c.is_ascii_alphanumeric() || "_'-".contains(c))
        };
        for pkg in self.packages() {
            // Keywords are no attribute names, except `or` after a dot
            // in some contexts, which gets rejected as well
            if !pkg
                .split('.')
                .all(|attr| identifier(attr) && !KEYWORDS.contains(&attr))
            {
                bail!(
                    "Invalid package '{}', expected a nixpkgs attribute path like 'jq' or \
                     'python3Packages.pyyaml'",
                    pkg,
                )
            }
        }
        Ok(())
    }

    /// Return the set shell as result type
    pub(crate) fn shell_ok(&self) -> Result<String> {
        let shell = self.shell.as_ref().context("No shell set")?;
        Ok(shell.into())
    }

    /// Returns true if multi node support is enabled
    pub(crate) fn multi_node(&self) -> bool {
        self.nodes() > 1
    }

    /// Create the run root and its parents, which group and others can
    /// read but not write. They need to read it to access the admin
    /// kubeconfig when running via sudo, and for `kubernix status`.
    fn create_root_dir(&self) -> Result<()> {
        DirBuilder::new()
            .recursive(true)
            .mode(0o755)
            .create(self.root())
            .with_context(|| {
                format!(
                    "Unable to create the run directory '{}'",
                    self.root().display()
                )
            })
    }
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use std::{
        ops::{Deref, DerefMut},
        path::Path,
    };
    use tempfile::{TempDir, tempdir};

    /// A configuration whose root is a temporary directory, which gets
    /// removed on drop.
    pub struct TestConfig {
        config: Config,
        _root: TempDir,
    }

    impl Deref for TestConfig {
        type Target = Config;

        fn deref(&self) -> &Config {
            &self.config
        }
    }

    impl DerefMut for TestConfig {
        fn deref_mut(&mut self) -> &mut Config {
            &mut self.config
        }
    }

    /// Parse the arguments (without the program name) like `from_args`,
    /// but without the environment, so that exported `KUBERNIX_*`
    /// variables do not matter.
    pub fn parse(args: &[&str]) -> Result<Config> {
        let matches = Config::command()
            .mut_args(|a| a.env(None::<&str>))
            .try_get_matches_from(["kubernix"].iter().chain(args))?;
        Ok(Config::from_matches(&matches)?)
    }

    /// Parse the arguments with the provided environment variables of the
    /// options, while all other ones are unset.
    fn parse_env(args: &[&str], vars: &[(&str, &str)]) -> Result<Config> {
        let vars: Vec<(String, Option<String>)> = Config::command()
            .get_arguments()
            .filter_map(|a| a.get_env())
            .map(|name| {
                let name = name.to_string_lossy().to_string();
                let value = vars
                    .iter()
                    .find(|(k, _)| *k == name)
                    .map(|(_, v)| v.to_string());
                (name, value)
            })
            .collect();
        temp_env::with_vars(vars, || {
            let matches =
                Config::command().try_get_matches_from(["kubernix"].iter().chain(args))?;
            Ok(Config::from_matches(&matches)?)
        })
    }

    fn strs(args: &[String]) -> Vec<&str> {
        args.iter().map(String::as_str).collect()
    }

    /// A default configuration with a new root.
    pub fn test_config() -> Result<TestConfig> {
        let root = tempdir()?;
        let mut config = parse(&["--root", root.path().to_str().context("invalid root")?])?;
        config.shell = Some("sh".into());
        config.canonicalize_root()?;
        Ok(TestConfig {
            config,
            _root: root,
        })
    }

    pub fn test_config_rootless() -> Result<TestConfig> {
        let mut c = test_config()?;
        c.set_rootless(true);
        Ok(c)
    }

    pub fn test_config_wrong_root() -> Result<TestConfig> {
        let mut c = test_config()?;
        c.root = Path::new("/").join("proc");
        Ok(c)
    }

    pub fn test_config_wrong_cidr() -> Result<TestConfig> {
        let mut c = test_config()?;
        c.cidr = "10.0.0.1/31".parse()?;
        Ok(c)
    }

    #[test]
    fn canonicalize_root_success() -> Result<()> {
        test_config()?.canonicalize_root()
    }

    #[test]
    fn canonicalize_root_failure() -> Result<()> {
        let mut c = Config {
            root: Path::new("/").join("proc").join("invalid"),
            ..parse(&[])?
        };
        assert!(c.canonicalize_root().is_err());
        Ok(())
    }

    #[test]
    fn check_trusted_root_real_dirs() -> Result<()> {
        use std::os::unix::fs::{PermissionsExt, symlink};
        let dir = tempdir()?;
        let uid = ::nix::unistd::Uid::effective().as_raw();
        let check = |root: &Path, owners: &[u32]| {
            Config::check_trusted_root(root, owners, DirAttrs::read, |_, _| false)
        };
        let mode = |path: &Path, mode| fs::set_permissions(path, fs::Permissions::from_mode(mode));
        let root = dir.path().join("root");
        fs::create_dir(&root)?;
        mode(&root, 0o755)?;
        mode(dir.path(), 0o755)?;
        // The parents of the temporary directory may belong to root
        check(&root, &[0, uid])?;

        // Writable by group or others
        for m in [0o775, 0o757, 0o1777] {
            mode(&root, m)?;
            let err = check(&root, &[0, uid]).unwrap_err();
            assert!(err.to_string().contains("run directory"), "{err}");
        }
        mode(&root, 0o700)?;
        check(&root, &[0, uid])?;

        // Not a directory
        let link = dir.path().join("link");
        symlink(&root, &link)?;
        assert!(check(&link, &[0, uid]).is_err());

        // A parent writable by others allows them to replace the root,
        // unless it has the sticky bit set.
        mode(dir.path(), 0o777)?;
        let err = check(&root, &[0, uid]).unwrap_err();
        assert!(err.to_string().contains("parent directory"), "{err}");
        mode(dir.path(), 0o1777)?;
        check(&root, &[0, uid])?;
        mode(dir.path(), 0o755)?;
        Ok(())
    }

    #[test]
    fn check_trusted_root_synthetic_dirs() -> Result<()> {
        use std::collections::BTreeMap;
        // uid 1000 runs kubernix via sudo and has the private group 1000,
        // uid 1001 is another user.
        let dir = |uid, gid, mode: u32| DirAttrs {
            dir: true,
            uid,
            gid,
            mode: 0o40000 | mode,
        };
        let check = |dirs: &BTreeMap<&str, DirAttrs>, owners: &[u32]| {
            Config::check_trusted_root(
                Path::new("/home/user/project/run"),
                owners,
                |path| {
                    dirs.get(path.to_str().context("invalid path")?)
                        .copied()
                        .context("not found")
                },
                |uid, gid| uid == 1000 && gid == 1000,
            )
        };
        let mut dirs = BTreeMap::from([
            ("/", dir(0, 0, 0o755)),
            ("/home", dir(0, 0, 0o755)),
            ("/home/user", dir(1000, 1000, 0o700)),
            ("/home/user/project", dir(1000, 1000, 0o775)),
            ("/home/user/project/run", dir(0, 0, 0o755)),
        ]);

        // A parent of the sudo user, writable by its private group
        check(&dirs, &[0, 1000])?;

        // Without sudo, the parents of the user cannot be trusted, and the
        // error explains why
        let err = check(&dirs, &[0]).unwrap_err().to_string();
        assert!(err.contains("'/home/user/project'"), "{err}");
        assert!(err.contains("SUDO_UID is not set"), "{err}");
        assert!(err.contains("as uid 1000"), "{err}");
        assert!(err.contains("--root /var/lib/kubernix"), "{err}");

        // A run directory of the sudo user
        dirs.insert("/home/user/project/run", dir(1000, 1000, 0o775));
        check(&dirs, &[0, 1000])?;

        // A group which is not the private one of the owner
        dirs.insert("/home/user/project", dir(1000, 100, 0o775));
        let err = check(&dirs, &[0, 1000]).unwrap_err();
        assert!(err.to_string().contains("parent directory"), "{err}");
        dirs.insert("/home/user/project", dir(1000, 100, 0o755));
        check(&dirs, &[0, 1000])?;
        dirs.insert("/home/user/project/run", dir(1000, 100, 0o775));
        let err = check(&dirs, &[0, 1000]).unwrap_err();
        assert!(err.to_string().contains("The run directory"), "{err}");
        dirs.insert("/home/user/project/run", dir(0, 0, 0o755));

        // Writable by others
        dirs.insert("/home/user/project", dir(1000, 1000, 0o757));
        assert!(check(&dirs, &[0, 1000]).is_err());

        // A parent of another user, even if it is not writable by others
        dirs.insert("/home/user/project", dir(1001, 1001, 0o755));
        let err = check(&dirs, &[0, 1000]).unwrap_err().to_string();
        assert!(err.contains("owner 1001"), "{err}");
        assert!(!err.contains("SUDO_UID"), "{err}");

        // A sticky parent may be writable by others, but has to be owned by
        // a trusted user
        dirs.insert("/home/user/project", dir(0, 0, 0o1777));
        check(&dirs, &[0, 1000])?;
        dirs.insert("/home/user/project", dir(1001, 1001, 0o1777));
        assert!(check(&dirs, &[0, 1000]).is_err());
        Ok(())
    }

    #[test]
    fn untrusted_owner_hint() {
        // Only without sudo and for owners other than root
        assert!(Config::untrusted_owner_hint(&[0], 1000).contains("SUDO_UID"));
        assert!(Config::untrusted_owner_hint(&[0], 0).is_empty());
        assert!(Config::untrusted_owner_hint(&[0, 1000], 1001).is_empty());
    }

    #[test]
    fn other_primary_member() {
        let passwd = "root:x:0:0:root:/root:/bin/bash\n\
                      user:x:1000:1000::/home/user:/bin/bash\n\
                      alias:x:1000:1000::/home/user:/bin/bash\n\
                      other:x:1001:1001::/home/other:/bin/bash\n\
                      broken\n\
                      \n";
        assert!(!DirAttrs::other_primary_member(passwd, 1000, 1000));
        assert!(DirAttrs::other_primary_member(
            &format!("{passwd}guest:x:1002:1000::/home/guest:/bin/sh\n"),
            1000,
            1000
        ));
        assert!(DirAttrs::other_primary_member(passwd, 1001, 1000));
        assert!(!DirAttrs::other_primary_member(passwd, 1000, 1002));
    }

    #[test]
    fn root_dir_created_not_writable_by_others() -> Result<()> {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempdir()?;
        let c = Config {
            root: dir.path().join("a").join("b"),
            ..parse(&[])?
        };
        c.create_root_dir()?;
        for path in [c.root(), &dir.path().join("a")] {
            assert_eq!(fs::metadata(path)?.permissions().mode() & 0o022, 0);
        }
        Ok(())
    }

    #[test]
    fn to_file_success() -> Result<()> {
        test_config()?.to_file()
    }

    #[test]
    fn to_file_failure() -> Result<()> {
        let c = Config {
            root: Path::new("/").join("proc").join("invalid"),
            ..parse(&[])?
        };
        assert!(c.to_file().is_err());
        Ok(())
    }

    #[test]
    fn load_file_success() -> Result<()> {
        let mut c = test_config()?;
        fs::write(
            c.root.join(Config::FILENAME),
            r#"
version = 2
addons = ["coredns"]
cidr = "1.1.1.1/16"
container-runtime = "podman"
cri-runtime = "crio"
log-format = "text"
log-level = "DEBUG"
no-shell = false
nodes = 1
packages = []
root = "root"
            "#,
        )?;
        let root = c.root.clone();
        assert!(c.load_file()?.is_none());
        assert_eq!(c.root(), root);
        assert_eq!(c.log_level(), LevelFilter::Debug);
        assert_eq!(c.log_format(), LogFormat::Text);
        // Host bits of older configurations are removed
        assert_eq!(&c.cidr().to_string(), "1.1.0.0/16");
        assert_eq!(c.cri_runtime(), CriRuntime::Crio);
        assert_eq!(c.oci_runtime(), OciRuntime::Crun);
        assert!(c.dockerfile().is_none());
        Ok(())
    }

    #[test]
    fn load_file_with_dockerfile() -> Result<()> {
        let mut c = test_config()?;
        fs::write(
            c.root.join(Config::FILENAME),
            r#"
version = 2
addons = ["coredns"]
cidr = "1.1.1.1/16"
container-runtime = "podman"
cri-runtime = "containerd"
dockerfile = "/tmp/MyDockerfile"
log-format = "json"
log-level = "DEBUG"
no-shell = false
nodes = 1
oci-runtime = "runc"
packages = []
root = "root"
            "#,
        )?;
        c.load_file()?;
        assert_eq!(c.log_format(), LogFormat::Json);
        assert_eq!(c.cri_runtime(), CriRuntime::Containerd);
        assert_eq!(c.oci_runtime(), OciRuntime::Runc);
        assert_eq!(c.dockerfile(), Some(Path::new("/tmp/MyDockerfile")));
        Ok(())
    }

    #[test]
    fn load_file_without_cri_runtime() -> Result<()> {
        let mut c = test_config()?;
        fs::write(
            c.root.join(Config::FILENAME),
            r#"
version = 2
addons = ["coredns"]
cidr = "1.1.1.1/16"
container-runtime = "podman"
log-format = "text"
log-level = "DEBUG"
no-shell = false
nodes = 1
packages = []
root = "root"
            "#,
        )?;
        c.load_file()?;
        assert_eq!(c.cri_runtime(), CriRuntime::Crio);
        assert_eq!(c.oci_runtime(), OciRuntime::Crun);
        Ok(())
    }

    #[test]
    fn oci_runtime_default_crun() -> Result<()> {
        assert_eq!(parse(&[])?.oci_runtime(), OciRuntime::Crun);
        Ok(())
    }

    #[test]
    fn oci_runtime_from_args() -> Result<()> {
        let c = parse(&["--oci-runtime", "runc"])?;
        assert_eq!(c.oci_runtime(), OciRuntime::Runc);
        let c = parse(&["--oci-runtime=crun"])?;
        assert_eq!(c.oci_runtime(), OciRuntime::Crun);
        Ok(())
    }

    #[test]
    fn oci_runtime_invalid() {
        assert!(parse(&["--oci-runtime", "youki"]).is_err());
    }

    #[test]
    fn oci_runtime_serialized() -> Result<()> {
        let mut c = test_config()?;
        c.oci_runtime = OciRuntime::Runc;
        let toml = toml::to_string(&*c)?;
        assert!(toml.contains(r#"oci-runtime = "runc""#));
        Ok(())
    }

    #[test]
    fn validate_default_success() -> Result<()> {
        test_config()?.validate()?;
        test_config_rootless()?.validate()
    }

    #[test]
    fn validate_runc_root_success() -> Result<()> {
        let mut c = test_config()?;
        c.oci_runtime = OciRuntime::Runc;
        c.validate()
    }

    #[test]
    fn validate_runc_rootless_success() -> Result<()> {
        let mut c = test_config_rootless()?;
        c.oci_runtime = OciRuntime::Runc;
        c.validate()
    }

    #[test]
    fn oci_runtime_names() {
        assert_eq!(OciRuntime::ALL, [OciRuntime::Crun, OciRuntime::Runc]);
        assert_eq!(OciRuntime::Crun.name(), "crun");
        assert_eq!(OciRuntime::Runc.name(), "runc");
    }

    #[test]
    fn load_file_failure() -> Result<()> {
        let mut c = test_config()?;
        assert!(c.load_file().is_err());
        fs::write(c.root.join(Config::FILENAME), "invalid")?;
        assert!(c.load_file().is_err());
        Ok(())
    }

    #[test]
    fn rootless_default_false() -> Result<()> {
        let c = test_config()?;
        assert!(!c.is_rootless());
        Ok(())
    }

    #[test]
    fn rootless_set_and_get() -> Result<()> {
        let c = test_config_rootless()?;
        assert!(c.is_rootless());
        Ok(())
    }

    #[test]
    fn validate_cidr_rootless_slirp4netns_overlap_failure() -> Result<()> {
        let mut c = test_config_rootless()?;
        c.cidr = "10.0.0.0/16".parse()?;
        let err = c.validate().unwrap_err().to_string();
        assert!(err.contains("overlaps with the slirp4netns network '10.0.2.0/24'"));
        Ok(())
    }

    #[test]
    fn validate_cidr_root_slirp4netns_overlap_success() -> Result<()> {
        let mut c = test_config()?;
        c.cidr = "10.0.0.0/16".parse()?;
        c.validate()
    }

    #[test]
    fn log_level_values() -> Result<()> {
        assert_eq!(parse(&["-l", "debug"])?.log_level(), LevelFilter::Debug);
        assert_eq!(parse(&["--log-level=WARN"])?.log_level(), LevelFilter::Warn);
        assert_eq!(parse(&["--log-level=off"])?.log_level(), LevelFilter::Off);
        assert!(parse(&["--log-level=verbose"]).is_err());
        Ok(())
    }

    #[test]
    fn hidden_short_aliases() -> Result<()> {
        // Short flags of older versions, which are no longer documented
        assert!(parse(&["-e"])?.no_shell());
        assert_eq!(parse(&["-f", "json"])?.log_format(), LogFormat::Json);
        let c = parse(&["-n", "2", "-u", "docker"])?;
        assert_eq!(c.container_runtime(), "docker");
        let help = Config::command().render_help().to_string();
        for short in ["-e,", "-f,", "-u,"] {
            assert!(!help.contains(short), "{}", help);
        }
        Ok(())
    }

    #[test]
    fn compat_policy() {
        assert_eq!(Compat::of(1, 1, 0), Compat::Current);
        assert_eq!(Compat::of(0, 1, 0), Compat::Migrate);
        assert_eq!(Compat::of(2, 1, 0), Compat::Newer);
        assert_eq!(Compat::of(0, 2, 1), Compat::Outdated);
        assert_eq!(Compat::of(1, 3, 1), Compat::Migrate);
        assert_eq!(Compat::of(2, 2, 2), Compat::Current);
    }

    #[test]
    fn version_written() -> Result<()> {
        let c = test_config()?;
        c.to_file()?;
        let content = read_to_string(c.root.join(Config::FILENAME))?;
        assert!(
            content.starts_with(&format!("version = {}\n", Config::VERSION)),
            "{}",
            content
        );
        Ok(())
    }

    #[test]
    fn version_current_loaded() -> Result<()> {
        let c = existing(&["--nodes=2"], &[])?;
        assert_eq!(c.version, Config::VERSION);
        assert_eq!(c.nodes(), 2);
        Ok(())
    }

    #[test]
    fn version_outdated_refused() -> Result<()> {
        // Version 0 (without the version) and 1 have the old CIDR layout and
        // PKI, which cannot be migrated
        let stored = test_config()?;
        stored.to_file()?;
        let file = stored.root.join(Config::FILENAME);
        let content = read_to_string(&file)?;
        let current = format!("version = {}\n", Config::VERSION);
        for version in ["", "version = 1\n"] {
            let old = content.replace(&current, version);
            assert_ne!(old, content);
            fs::write(&file, &old)?;

            let mut c = test_config()?;
            c.root = stored.root.clone();
            let err = c.load_file().err().context("expected error")?.to_string();
            assert!(err.contains("has format version"), "{}", err);
            assert!(err.contains("cannot be migrated"), "{}", err);
            let remove = format!("sudo rm -rf '{}'", c.root().display());
            assert!(err.ends_with(&remove), "{}", err);
            assert_eq!(read_to_string(&file)?, old);
        }
        Ok(())
    }

    #[test]
    fn version_newer_rejected() -> Result<()> {
        let mut c = test_config()?;
        let file = c.root.join(Config::FILENAME);
        // Newer versions may change the format of the other settings
        let content = format!("version = {}\nnodes = \"many\"\n", Config::VERSION + 1);
        fs::write(&file, &content)?;
        let err = c.load_file().err().context("expected error")?.to_string();
        assert!(err.contains("of a newer kubernix"), "{}", err);
        assert!(err.contains("remove the run directory"), "{}", err);
        assert_eq!(read_to_string(&file)?, content);
        Ok(())
    }

    #[test]
    fn check_version_bounds() -> Result<()> {
        let c = test_config()?;
        let err = c.check_version(u32::MAX, false).unwrap_err().to_string();
        assert!(err.contains("newer kubernix"), "{}", err);
        assert!(!c.check_version(Config::VERSION, false)?);
        assert_eq!(
            c.check_version(Config::OLDEST_VERSION, false)?,
            Config::OLDEST_VERSION < Config::VERSION
        );

        // Rootless clusters leave files of subordinate users behind
        let err = c.check_version(0, true).unwrap_err().to_string();
        let remove = format!(
            "unshare --map-auto --map-root-user rm -rf '{}'",
            c.root().display()
        );
        assert!(err.ends_with(&remove), "{}", err);
        Ok(())
    }

    #[test]
    fn packages_from_env_comma_separated() -> Result<()> {
        let c = parse_env(&[], &[("KUBERNIX_PACKAGES", "kubernetes-helm,jq")])?;
        assert_eq!(c.packages(), ["kubernetes-helm", "jq"]);
        assert!(c.is_explicit("packages"));
        let c = parse_env(&[], &[("KUBERNIX_PACKAGES", "")])?;
        assert!(c.packages().is_empty());
        assert!(c.is_explicit("packages"));
        Ok(())
    }

    #[test]
    fn packages_from_args() -> Result<()> {
        for args in [
            &["-p", "helm", "-p", "jq"][..],
            &["-p", "helm,jq"],
            &["--packages=helm,jq"],
        ] {
            assert_eq!(parse(args)?.packages(), ["helm", "jq"]);
        }
        let c = parse(&["--packages="])?;
        assert!(c.packages().is_empty());
        assert!(c.is_explicit("packages"));

        // Values are not separated by spaces, so they do not swallow the
        // subcommand
        let c = parse(&["-p", "jq", "status"])?;
        assert_eq!(c.packages(), ["jq"]);
        assert!(matches!(c.subcommand, Some(SubCommand::Status { .. })));
        assert!(parse(&["-p", "helm", "jq"]).is_err());
        Ok(())
    }

    #[test]
    fn addons_from_env_comma_separated() -> Result<()> {
        let c = parse_env(&[], &[("KUBERNIX_ADDONS", "coredns,coredns")])?;
        assert_eq!(c.addons(), [Addon::Coredns, Addon::Coredns]);
        let c = parse_env(&[], &[("KUBERNIX_ADDONS", "none")])?;
        assert!(c.addons().is_empty());
        Ok(())
    }

    #[test]
    fn addons_from_args() -> Result<()> {
        let c = parse(&["-a", "coredns", "-a", "coredns"])?;
        assert_eq!(c.addons(), [Addon::Coredns, Addon::Coredns]);
        assert_eq!(parse(&[])?.addons(), [Addon::Coredns]);
        assert_eq!(parse(&["--addons=coredns"])?.addons(), [Addon::Coredns]);
        assert!(parse(&["--addons=none"])?.addons().is_empty());
        assert!(parse(&["--addons", "none"])?.addons().is_empty());
        assert!(parse(&["-a"]).is_err());
        assert!(parse(&["--addons=coredns,invalid"]).is_err());
        let err = parse(&["--addons=coredns,none"])
            .err()
            .context("expected error")?
            .to_string();
        assert!(err.contains("cannot be combined"), "{}", err);

        let c = parse(&["-a", "coredns", "status"])?;
        assert_eq!(c.addons(), [Addon::Coredns]);
        assert!(matches!(c.subcommand, Some(SubCommand::Status { .. })));
        Ok(())
    }

    #[test]
    fn multi_node_threshold() -> Result<()> {
        let mut c = test_config()?;
        assert!(!c.multi_node());
        c.nodes = 2;
        assert!(c.multi_node());
        Ok(())
    }

    #[test]
    fn rootless_serialized() -> Result<()> {
        let mut c = test_config()?;
        assert!(!toml::to_string(&*c)?.contains("rootless"));
        c.set_rootless(true);
        assert!(toml::to_string(&*c)?.contains("rootless = true"));
        Ok(())
    }

    #[test]
    fn load_file_preserves_rootless() -> Result<()> {
        let mut c = test_config()?;
        c.to_file()?;
        c.set_rootless(true);
        c.load_file()?;
        assert!(c.is_rootless());
        Ok(())
    }

    #[test]
    fn check_stored_mode() -> Result<()> {
        use std::os::unix::fs::symlink;
        let mut c = test_config_rootless()?;
        c.to_file()?;

        // Root refuses a rootless run directory, for the subcommands too
        for subcommand in [
            None,
            Some(SubCommand::Status {
                output: OutputFormat::Text,
            }),
        ] {
            c.set_rootless(false);
            c.subcommand = subcommand;
            let err = c.check_stored_mode().unwrap_err();
            assert!(
                err.to_string().contains("belongs to a rootless cluster"),
                "{err}"
            );
        }

        // The same mode passes
        c.set_rootless(true);
        c.check_stored_mode()?;

        // Other users can read the state of a root cluster
        c.set_rootless(false);
        c.to_file()?;
        c.set_rootless(true);
        c.subcommand = Some(SubCommand::Stop { timeout: 1 });
        c.check_stored_mode()?;
        assert!(!c.is_rootless());

        // Symbolic links and unparsable files are left to load_file
        let other = tempdir()?;
        let target = other.path().join("kubernix.toml");
        fs::write(&target, "rootless = true\n")?;
        fs::remove_file(c.file())?;
        symlink(&target, c.file())?;
        c.set_rootless(false);
        c.subcommand = None;
        c.check_stored_mode()?;
        fs::remove_file(c.file())?;
        fs::write(c.file(), "rootless = \"invalid\"\n")?;
        c.check_stored_mode()?;
        Ok(())
    }

    /// Parse the arguments like `from_args` and use a new root, which
    /// contains the configuration of a previous run with `stored_args`.
    fn existing(stored_args: &[&str], args: &[&str]) -> Result<TestConfig> {
        existing_env(stored_args, args, &[])
    }

    /// Like `existing`, with the environment variables of the options set
    /// to `vars` for the current run.
    fn existing_env(
        stored_args: &[&str],
        args: &[&str],
        vars: &[(&str, &str)],
    ) -> Result<TestConfig> {
        let root = tempdir()?;
        let path = root.path().to_str().context("invalid root")?;
        let with_root = |args: &[&str]| -> Vec<String> {
            ["--root", path]
                .iter()
                .chain(args)
                .map(ToString::to_string)
                .collect()
        };
        parse(&strs(&with_root(stored_args)))?.to_file()?;
        let mut config = parse_env(&strs(&with_root(args)), vars)?;
        config.load_file()?;
        Ok(TestConfig {
            config,
            _root: root,
        })
    }

    #[test]
    fn from_matches_explicit() -> Result<()> {
        let c = parse(&["--nodes=2"])?;
        assert!(c.is_explicit("nodes"));
        assert!(!c.is_explicit("cidr"));
        assert!(!c.is_explicit("root"));
        Ok(())
    }

    #[test]
    fn from_matches_explicit_env() -> Result<()> {
        let c = parse_env(&[], &[("KUBERNIX_LOG_LEVEL", "debug")])?;
        assert!(c.is_explicit("log_level"));
        assert_eq!(c.log_level(), LevelFilter::Debug);
        Ok(())
    }

    #[test]
    fn shell_options_precedence() -> Result<()> {
        // The command line wins over the environment
        let c = parse_env(&["--shell=bash"], &[("KUBERNIX_NO_SHELL", "true")])?;
        assert_eq!(c.shell(), Some("bash"));
        assert!(!c.no_shell());
        assert!(!c.is_explicit("no_shell"));
        let c = parse_env(&["--no-shell"], &[("KUBERNIX_SHELL", "zsh")])?;
        assert_eq!(c.shell(), None);
        assert!(c.no_shell());
        assert!(!c.is_explicit("shell"));

        // A disabled option is not set
        let c = parse_env(&["--shell=bash"], &[("KUBERNIX_NO_SHELL", "false")])?;
        assert_eq!(c.shell(), Some("bash"));
        assert!(!c.is_explicit("no_shell"));

        // Both from the same source conflict
        assert!(parse(&["--no-shell", "--shell=sh"]).is_err());
        let vars = [("KUBERNIX_NO_SHELL", "true"), ("KUBERNIX_SHELL", "zsh")];
        assert!(parse_env(&[], &vars).is_err());
        Ok(())
    }

    #[test]
    fn container_runtime_without_nodes() -> Result<()> {
        let vars = [("KUBERNIX_CONTAINER_RUNTIME", "docker")];
        assert_eq!(parse_env(&["status"], &vars)?.container_runtime(), "docker");
        let c = parse(&["--container-runtime=docker"])?;
        assert_eq!(c.container_runtime(), "docker");
        Ok(())
    }

    #[test]
    fn cidr_without_host_bits() -> Result<()> {
        let c = parse(&["--cidr=10.10.0.1/16"])?;
        assert_eq!(c.cidr().to_string(), "10.10.0.0/16");
        let c = existing(&["--cidr=10.20.0.0/16"], &["--cidr=10.20.3.4/16"])?;
        assert_eq!(c.cidr().to_string(), "10.20.0.0/16");
        assert!(parse(&["--cidr=10.10.0.0/33"]).is_err());
        Ok(())
    }

    #[test]
    fn empty_paths_unset() -> Result<()> {
        let c = parse(&["--overlay=", "--dockerfile="])?;
        assert_eq!(c.overlay(), None);
        assert_eq!(c.dockerfile(), None);
        assert!(c.is_explicit("overlay"));
        let vars = [("KUBERNIX_OVERLAY", ""), ("KUBERNIX_DOCKERFILE", "")];
        let c = parse_env(&[], &vars)?;
        assert_eq!(c.overlay(), None);
        assert!(c.is_explicit("dockerfile"));

        // Empty values override the stored ones
        let stored = ["--overlay=/o.nix", "--dockerfile=/D", "--packages=jq"];
        let c = existing(&stored, &[])?;
        assert_eq!(c.overlay(), Some(Path::new("/o.nix")));
        assert_eq!(c.dockerfile(), Some(Path::new("/D")));
        assert_eq!(c.packages(), ["jq"]);
        let c = existing(&stored, &["--overlay=", "--packages="])?;
        assert_eq!(c.overlay(), None);
        assert_eq!(c.dockerfile(), Some(Path::new("/D")));
        assert!(c.packages().is_empty());
        let c = existing_env(&stored, &[], &vars)?;
        assert_eq!(c.overlay(), None);
        assert_eq!(c.dockerfile(), None);
        Ok(())
    }

    #[test]
    fn validate_layout() -> Result<()> {
        let mut c = test_config()?;
        c.cidr = "10.10.0.0/30".parse()?;
        assert!(c.validate().is_err());
        c.cidr = "10.10.0.0/24".parse()?;
        c.nodes = 64;
        assert!(c.validate().is_err());
        c.nodes = 8;
        c.validate()
    }

    #[test]
    fn validate_packages() -> Result<()> {
        let mut c = test_config()?;
        for pkg in ["jq", "kubernetes-helm", "python3Packages.pyyaml", "_a'"] {
            c.packages = vec![pkg.into()];
            c.validate()?;
        }
        for pkg in [
            "1password",
            "a..b",
            ".a",
            "a.",
            "let",
            "foo.in",
            "python3Packages.let.x",
            "a.or",
            "a b",
            "jq;",
            "a/b",
            "",
        ] {
            c.packages = vec![pkg.into()];
            let err = c.validate().err().context(pkg)?.to_string();
            assert!(err.contains("Invalid package"), "{}", err);
        }
        Ok(())
    }

    #[test]
    fn validate_paths() -> Result<()> {
        let mut c = test_config()?;
        let file = c.root().join("overlay.nix");
        c.overlay = Some(file.clone());
        let err = c.validate().err().context("expected error")?.to_string();
        assert!(err.contains("The overlay"), "{}", err);
        fs::write(&file, "")?;
        c.validate()?;

        c.dockerfile = Some(c.root().to_path_buf());
        let err = c.validate().err().context("expected error")?.to_string();
        assert!(err.contains("The Dockerfile"), "{}", err);
        Ok(())
    }

    #[test]
    fn remove_option_env_removes_all() {
        let mut cmd = Command::new("true");
        Config::remove_option_env(&mut cmd);
        let removed: Vec<String> = cmd
            .get_envs()
            .filter(|(_, v)| v.is_none())
            .map(|(k, _)| k.to_string_lossy().to_string())
            .collect();
        assert!(removed.contains(&"KUBERNIX_ROOT".into()));
        assert!(removed.contains(&"KUBERNIX_NO_SHELL".into()));
        assert_eq!(removed.len(), 14, "{:?}", removed);
    }

    #[test]
    fn merge_keeps_stored_cluster_settings() -> Result<()> {
        let c = existing(&["--nodes=2", "--cri-runtime=containerd"], &[])?;
        assert_eq!(c.nodes(), 2);
        assert_eq!(c.cri_runtime(), CriRuntime::Containerd);
        Ok(())
    }

    #[test]
    fn merge_accepts_equal_cluster_settings() -> Result<()> {
        let c = existing(&["--nodes=2"], &["--nodes=2", "--oci-runtime=crun"])?;
        assert_eq!(c.nodes(), 2);
        Ok(())
    }

    #[test]
    fn merge_ignores_cluster_settings_for_subcommands() -> Result<()> {
        let stored = ["--nodes=2", "--cri-runtime=containerd"];
        for subcommand in ["status", "stop", "shell"] {
            let args = [
                "--nodes=3",
                "--cidr=10.20.0.0/16",
                "--cri-runtime=crio",
                "--oci-runtime=runc",
                "--container-runtime=docker",
                subcommand,
            ];
            let c = existing(&stored, &args)?;
            assert_eq!(c.nodes(), 2);
            assert_eq!(c.cidr().to_string(), "10.10.0.0/16");
            assert_eq!(c.cri_runtime(), CriRuntime::Containerd);
            assert_eq!(c.oci_runtime(), OciRuntime::Crun);
            assert_eq!(c.container_runtime(), "podman");
        }
        Ok(())
    }

    #[test]
    fn merge_rejects_different_cluster_settings() -> Result<()> {
        for (stored, args, name) in [
            (&["--nodes=2"][..], &["--nodes=3"][..], "nodes"),
            (&[], &["--cidr=10.20.0.0/16"], "cidr"),
            (&[], &["--cri-runtime=containerd"], "cri-runtime"),
            (
                &["--oci-runtime=runc"],
                &["--oci-runtime=crun"],
                "oci-runtime",
            ),
        ] {
            let err = existing(stored, args)
                .err()
                .context("expected error")?
                .to_string();
            assert!(err.contains(&format!("The {} setting", name)), "{}", err);
            assert!(err.contains("remove the run directory"), "{}", err);
        }
        Ok(())
    }

    #[test]
    fn merge_overrides_explicit_settings() -> Result<()> {
        let stored = ["--log-level=debug", "--log-format=json", "--no-shell"];
        let c = existing(&stored, &[])?;
        assert_eq!(c.log_level(), LevelFilter::Debug);
        assert_eq!(c.log_format(), LogFormat::Json);
        assert!(c.no_shell());

        let c = existing(&stored, &["--log-level=warn", "--shell=sh"])?;
        assert_eq!(c.log_level(), LevelFilter::Warn);
        assert_eq!(c.log_format(), LogFormat::Json);
        assert_eq!(c.shell(), Some("sh"));
        assert!(!c.no_shell());
        Ok(())
    }

    #[test]
    fn merge_overrides_nix_environment() -> Result<()> {
        let stored = ["--packages=hello"];
        let c = existing(&stored, &[])?;
        assert_eq!(c.packages(), ["hello"]);

        let c = existing(&stored, &["--packages", "hello,kubernetes-helm"])?;
        assert_eq!(c.packages(), ["hello", "kubernetes-helm"]);
        Ok(())
    }

    #[test]
    fn merge_overrides_nix_environment_from_env() -> Result<()> {
        let vars = [
            ("KUBERNIX_PACKAGES", "kubernetes-helm,jq"),
            ("KUBERNIX_ADDONS", "coredns"),
        ];
        let c = existing_env(&["--packages=hello", "--addons=none"], &[], &vars)?;
        assert_eq!(c.packages(), ["kubernetes-helm", "jq"]);
        assert_eq!(c.addons(), [Addon::Coredns]);
        Ok(())
    }

    #[test]
    fn merge_ignores_stored_root() -> Result<()> {
        let mut c = test_config()?;
        let root = c.root.clone();
        let mut stored = test_config()?;
        stored.root = PathBuf::from("relative");
        c.merge(stored.clone())?;
        assert_eq!(c.root(), root);
        Ok(())
    }

    #[test]
    fn merge_rejects_rootless_mismatch() -> Result<()> {
        let mut stored = test_config()?;
        stored.set_rootless(true);
        let mut c = test_config()?;
        c.set_rootless(false);
        let err = c.merge(stored.clone()).unwrap_err().to_string();
        assert!(err.contains("belongs to a rootless cluster"), "{}", err);

        // Configurations of older versions do not contain the mode
        stored.rootless = None;
        c.merge(stored.clone())
    }

    #[test]
    fn merge_suggests_sudo_for_root_cluster() -> Result<()> {
        let mut stored = test_config()?;
        stored.set_rootless(false);
        let mut c = test_config()?;
        c.set_rootless(true);
        let err = c.merge(stored.clone()).unwrap_err().to_string();
        assert!(err.contains("belongs to a root cluster"), "{}", err);

        c.subcommand = Some(SubCommand::Shell { command: vec![] });
        let err = c.merge(stored.clone()).unwrap_err().to_string();
        assert!(err.contains("runs as root, use 'sudo -E env"), "{}", err);

        // Stop and status work on the root cluster, for example to report
        // that it is stopped
        for subcommand in [
            SubCommand::Stop { timeout: 1 },
            SubCommand::Status {
                output: OutputFormat::Text,
            },
        ] {
            c.set_rootless(true);
            c.subcommand = Some(subcommand);
            c.merge(stored.clone())?;
            assert!(!c.is_rootless());
        }

        // A rootless cluster cannot be used as root
        stored.set_rootless(true);
        c.set_rootless(false);
        let err = c.merge(stored.clone()).unwrap_err().to_string();
        assert!(err.contains("belongs to a rootless cluster"), "{}", err);
        Ok(())
    }

    #[test]
    fn sudo_command_escapes_args() {
        let args = ["shell", "--", "sh", "-c", "echo 'hi'"].map(String::from);
        assert_eq!(
            Config::sudo_command(Path::new("/bin/kubernix"), &args),
            r#"sudo -E env "PATH=$PATH" '/bin/kubernix' 'shell' '--' 'sh' '-c' 'echo '\''hi'\'''"#
        );
    }

    #[test]
    fn subcommands_from_args() -> Result<()> {
        let subcommand =
            |args: &[&str]| -> Result<Option<SubCommand>> { Ok(parse(&args[1..])?.subcommand) };
        assert_eq!(subcommand(&["kubernix"])?, None);
        assert_eq!(
            subcommand(&["kubernix", "shell"])?,
            Some(SubCommand::Shell { command: vec![] })
        );
        assert_eq!(
            subcommand(&["kubernix", "shell", "--", "kubectl", "get", "-o", "wide"])?,
            Some(SubCommand::Shell {
                command: ["kubectl", "get", "-o", "wide"].map(String::from).into()
            })
        );
        assert!(subcommand(&["kubernix", "shell", "kubectl"]).is_err());
        assert_eq!(
            subcommand(&["kubernix", "stop"])?,
            Some(SubCommand::Stop { timeout: 180 })
        );
        assert_eq!(
            subcommand(&["kubernix", "stop", "--timeout=5", "--root=/tmp"])?,
            Some(SubCommand::Stop { timeout: 5 })
        );
        assert_eq!(
            subcommand(&["kubernix", "status"])?,
            Some(SubCommand::Status {
                output: OutputFormat::Text
            })
        );
        assert_eq!(
            subcommand(&["kubernix", "status", "--output", "json"])?,
            Some(SubCommand::Status {
                output: OutputFormat::Json
            })
        );
        assert!(subcommand(&["kubernix", "status", "--output", "yaml"]).is_err());
        Ok(())
    }
}
