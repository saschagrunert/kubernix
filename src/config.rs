//! Configuration related structures
use crate::{podman::Podman, system::System};
use anyhow::{Context, Result, bail};
use clap::{
    ArgMatches, CommandFactory, FromArgMatches, Parser, Subcommand as ClapSubcommand, ValueEnum,
    builder::{PossibleValuesParser, TypedValueParser, styling},
    error::ErrorKind,
    parser::ValueSource,
};
use ipnetwork::Ipv4Network;
use log::LevelFilter;
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeSet,
    fmt,
    fs::{self, canonicalize, create_dir_all, read_to_string},
    net::Ipv4Addr,
    path::{Path, PathBuf, absolute},
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
    /// Human-readable colored text (default)
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
    /// CRI-O container runtime (default)
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
    /// crun OCI runtime (default)
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
    /// CoreDNS for the cluster DNS (default)
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
        value_name = "CIDR"
    )]
    /// The CIDR used for the cluster
    cidr: Ipv4Network,

    #[arg(
        env = "KUBERNIX_OVERLAY",
        long = "overlay",
        short = 'o',
        value_name = "PATH"
    )]
    /// The Nix package overlay to be used
    overlay: Option<PathBuf>,

    #[arg(
        env = "KUBERNIX_PACKAGES",
        long = "packages",
        num_args = 1..,
        short = 'p',
        value_delimiter = ',',
        value_name = "PACKAGE",
    )]
    /// Additional Nix packages to be added to the environment, comma separated
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
        requires = "nodes",
        short_alias = 'u',
        value_name = "EXECUTABLE",
    )]
    /// Container runtime for the node containers, only used in multinode root mode
    container_runtime: String,

    #[arg(
        conflicts_with = "shell",
        env = "KUBERNIX_NO_SHELL",
        long = "no-shell",
        short_alias = 'e'
    )]
    /// Do not spawn an interactive shell after bootstrap
    no_shell: bool,

    #[arg(
        env = "KUBERNIX_DOCKERFILE",
        long = "dockerfile",
        short = 'd',
        value_name = "PATH"
    )]
    /// Custom Dockerfile path for multi-node container image builds
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
        num_args = 0..,
        value_delimiter = ',',
        value_name = "ADDON",
    )]
    /// Cluster addons to deploy, comma separated, `none` disables all
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

    /// Parent CIDR from which cluster, service, and pod subnets are carved.
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
#[derive(Clone, ClapSubcommand, Deserialize, Serialize)]
pub enum SubCommand {
    /// Spawn an additional shell session
    #[command(name = "shell")]
    Shell,
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
        config.overlay = config.overlay.map(Self::absolute);
        config.dockerfile = config.dockerfile.map(Self::absolute);
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
}

impl Config {
    const FILENAME: &'static str = "kubernix.toml";

    /// Format version of new run directories and configuration files.
    ///
    /// - 0: Run directories created before the versioning
    /// - 1: Adds the version, otherwise identical to 0
    ///
    /// Bump it if an existing run directory or configuration file cannot be
    /// used as is anymore, and either migrate the older versions in
    /// `migrate` or refuse them by raising `OLDEST_VERSION`.
    pub(crate) const VERSION: u32 = 1;

    /// Oldest version which gets migrated in place to `VERSION`. Older
    /// run directories are refused and have to be removed.
    const OLDEST_VERSION: u32 = 0;

    /// Check the version of a stored configuration file. Returns true if
    /// the run directory has to be migrated.
    fn check_version(&self, version: u32) -> Result<bool> {
        match Compat::of(version, Self::VERSION, Self::OLDEST_VERSION) {
            Compat::Current => Ok(false),
            Compat::Migrate => Ok(true),
            Compat::Outdated => bail!(
                "The run directory '{}' has been created by an older kubernix (version {}, \
                 supported are {} to {}) and cannot be migrated, remove the run directory",
                self.root().display(),
                version,
                Self::OLDEST_VERSION,
                Self::VERSION,
            ),
            Compat::Newer => bail!(
                "The run directory '{}' has been created by a newer kubernix (version {}, \
                 supported up to {}), upgrade kubernix or remove the run directory",
                self.root().display(),
                version,
                Self::VERSION,
            ),
        }
    }

    /// Migrate the stored configuration of an older run directory to the
    /// current version.
    fn migrate(mut stored: Config) -> Config {
        // Version 0 to 1 only adds the version
        stored.version = Self::VERSION;
        stored
    }

    /// Make the configs root path absolute
    pub(crate) fn canonicalize_root(&mut self) -> Result<()> {
        self.create_root_dir()?;
        self.root = canonicalize(self.root()).with_context(|| {
            format!(
                "Unable to canonicalize config root directory '{}'",
                self.root().display()
            )
        })?;
        Ok(())
    }

    /// Write the current configuration to the internal set root path
    pub(crate) fn to_file(&self) -> Result<()> {
        self.create_root_dir()?;
        fs::write(self.root().join(Self::FILENAME), toml::to_string(&self)?)
            .context("Unable to write configuration to file")?;
        Ok(())
    }

    /// Returns true if the configuration file exists in the root path.
    pub(crate) fn file_exists(&self) -> bool {
        self.root().join(Self::FILENAME).exists()
    }

    /// Read the configuration from the internal set root path and merge
    /// it into the current one, after migrating it from an older version
    /// if needed. If not existing, write the current configuration to the
    /// path.
    pub(crate) fn try_load_file(&mut self) -> Result<()> {
        let file = self.root().join(Self::FILENAME);
        if file.exists() {
            let content = read_to_string(&file).with_context(|| {
                format!(
                    "Unable to read expected configuration file '{}'",
                    file.display(),
                )
            })?;
            let load_err = || format!("Unable to load config file '{}'", file.display());
            let version = toml::from_str::<Version>(&content).with_context(load_err)?;
            let migrate = self.check_version(version.version)?;
            let stored: Config = toml::from_str(&content).with_context(load_err)?;
            if migrate {
                let stored = Self::migrate(stored);
                self.merge(stored.clone())?;
                fs::write(&file, toml::to_string(&stored)?).with_context(|| {
                    format!("Unable to write migrated config file '{}'", file.display())
                })?;
            } else {
                self.merge(stored)?;
            }
        } else {
            self.to_file()?;
        }
        Ok(())
    }

    /// Merge the stored configuration of an existing run directory.
    ///
    /// Settings which shape the cluster have to match the stored ones,
    /// since the existing cluster state depends on them. The other settings
    /// are taken from the stored configuration, unless they have been set
    /// explicitly. The root and the rootless mode are never taken from the
    /// stored configuration.
    fn merge(&mut self, mut stored: Config) -> Result<()> {
        let mode = |rootless| if rootless { "rootless" } else { "root" };
        if let Some(rootless) = stored.rootless
            && rootless != self.is_rootless()
        {
            bail!(
                "The run directory '{}' belongs to a {} cluster and cannot be used in {} mode, \
                 use a different root or remove the run directory",
                self.root().display(),
                mode(rootless),
                mode(self.is_rootless()),
            )
        }

        stored.overlay = stored.overlay.map(Self::absolute);
        stored.dockerfile = stored.dockerfile.map(Self::absolute);

        macro_rules! cluster {
            ($($field:ident),*) => {$(
                if self.is_explicit(stringify!($field)) && self.$field != stored.$field {
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

    /// Validate option combinations which are not supported.
    pub(crate) fn validate(&self) -> Result<()> {
        if self.is_rootless() && self.cidr().overlaps(SLIRP4NETNS_CIDR) {
            bail!(
                "The CIDR '{}' overlaps with the slirp4netns network '{}', \
                 which provides the network connectivity in rootless mode",
                self.cidr(),
                SLIRP4NETNS_CIDR,
            )
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

    fn create_root_dir(&self) -> Result<()> {
        create_dir_all(self.root()).with_context(|| {
            format!(
                "Unable to create root directory '{}'",
                self.root().display()
            )
        })
    }
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use std::path::Path;
    use tempfile::tempdir;

    pub fn test_config() -> Result<Config> {
        let mut c = Config::parse_from(&[] as &[&str]);
        if c.shell.is_none() {
            c.shell = System::shell().ok();
        }
        c.root = tempdir()?.keep();
        c.canonicalize_root()?;
        Ok(c)
    }

    pub fn test_config_rootless() -> Result<Config> {
        let mut c = test_config()?;
        c.set_rootless(true);
        Ok(c)
    }

    pub fn test_config_wrong_root() -> Result<Config> {
        let mut c = test_config()?;
        c.root = Path::new("/").join("proc");
        Ok(c)
    }

    pub fn test_config_wrong_cidr() -> Result<Config> {
        let mut c = test_config()?;
        c.cidr = "10.0.0.1/31".parse()?;
        Ok(c)
    }

    #[test]
    fn canonicalize_root_success() -> Result<()> {
        let mut c = Config {
            root: tempdir()?.keep(),
            ..Config::parse_from([""; 0])
        };
        c.canonicalize_root()
    }

    #[test]
    fn canonicalize_root_failure() {
        let mut c = Config {
            root: Path::new("/").join("proc").join("invalid"),
            ..Config::parse_from([""; 0])
        };
        assert!(c.canonicalize_root().is_err())
    }

    #[test]
    fn to_file_success() -> Result<()> {
        let c = Config {
            root: tempdir()?.keep(),
            ..Config::parse_from([""; 0])
        };
        c.to_file()
    }

    #[test]
    fn to_file_failure() {
        let c = Config {
            root: Path::new("/").join("proc").join("invalid"),
            ..Config::parse_from([""; 0])
        };
        assert!(c.to_file().is_err())
    }

    #[test]
    fn try_load_file_success() -> Result<()> {
        let mut c = Config {
            root: tempdir()?.keep(),
            ..Config::parse_from([""; 0])
        };
        fs::write(
            c.root.join(Config::FILENAME),
            r#"
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
        c.try_load_file()?;
        assert_eq!(c.root(), root);
        assert_eq!(c.log_level(), LevelFilter::Debug);
        assert_eq!(c.log_format(), LogFormat::Text);
        assert_eq!(&c.cidr().to_string(), "1.1.1.1/16");
        assert_eq!(c.cri_runtime(), CriRuntime::Crio);
        assert_eq!(c.oci_runtime(), OciRuntime::Crun);
        assert!(c.dockerfile().is_none());
        Ok(())
    }

    #[test]
    fn try_load_file_with_dockerfile() -> Result<()> {
        let mut c = Config {
            root: tempdir()?.keep(),
            ..Config::parse_from([""; 0])
        };
        fs::write(
            c.root.join(Config::FILENAME),
            r#"
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
        c.try_load_file()?;
        assert_eq!(c.log_format(), LogFormat::Json);
        assert_eq!(c.cri_runtime(), CriRuntime::Containerd);
        assert_eq!(c.oci_runtime(), OciRuntime::Runc);
        assert_eq!(c.dockerfile(), Some(Path::new("/tmp/MyDockerfile")));
        Ok(())
    }

    #[test]
    fn try_load_file_without_cri_runtime() -> Result<()> {
        let mut c = Config {
            root: tempdir()?.keep(),
            ..Config::parse_from([""; 0])
        };
        fs::write(
            c.root.join(Config::FILENAME),
            r#"
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
        c.try_load_file()?;
        assert_eq!(c.cri_runtime(), CriRuntime::Crio);
        assert_eq!(c.oci_runtime(), OciRuntime::Crun);
        Ok(())
    }

    #[test]
    fn oci_runtime_default_crun() {
        let c = Config::parse_from([""; 0]);
        assert_eq!(c.oci_runtime(), OciRuntime::Crun);
    }

    #[test]
    fn oci_runtime_from_args() {
        let c = Config::parse_from(["kubernix", "--oci-runtime", "runc"]);
        assert_eq!(c.oci_runtime(), OciRuntime::Runc);
        let c = Config::parse_from(["kubernix", "--oci-runtime=crun"]);
        assert_eq!(c.oci_runtime(), OciRuntime::Crun);
    }

    #[test]
    fn oci_runtime_invalid() {
        assert!(Config::try_parse_from(["kubernix", "--oci-runtime", "youki"]).is_err());
    }

    #[test]
    fn oci_runtime_serialized() -> Result<()> {
        let mut c = test_config()?;
        c.oci_runtime = OciRuntime::Runc;
        let toml = toml::to_string(&c)?;
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
    fn try_load_file_failure() -> Result<()> {
        let mut c = Config {
            root: tempdir()?.keep(),
            ..Config::parse_from([""; 0])
        };
        fs::write(c.root.join(Config::FILENAME), "invalid")?;
        assert!(c.try_load_file().is_err());
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

    fn parse(args: &[&str]) -> Result<Config> {
        let matches = Config::command().try_get_matches_from(["kubernix"].iter().chain(args))?;
        Ok(Config::from_matches(&matches)?)
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
    fn version_missing_migrated() -> Result<()> {
        // Configuration files of older versions do not contain the version
        let mut stored = test_config()?;
        stored.cidr = "10.20.0.0/16".parse()?;
        stored.to_file()?;
        let file = stored.root.join(Config::FILENAME);
        let content = read_to_string(&file)?;
        let old = content.replace(&format!("version = {}\n", Config::VERSION), "");
        assert_ne!(old, content);
        fs::write(&file, &old)?;

        let mut c = test_config()?;
        c.root = stored.root.clone();
        c.try_load_file()?;
        assert_eq!(c.version, Config::VERSION);
        assert_eq!(&c.cidr().to_string(), "10.20.0.0/16");

        // The stored configuration got only the version added
        assert_eq!(read_to_string(&file)?, content);
        Ok(())
    }

    #[test]
    fn version_newer_rejected() -> Result<()> {
        let mut c = test_config()?;
        let file = c.root.join(Config::FILENAME);
        // Newer versions may change the format of the other settings
        let content = format!("version = {}\nnodes = \"many\"\n", Config::VERSION + 1);
        fs::write(&file, &content)?;
        let err = c.try_load_file().unwrap_err().to_string();
        assert!(err.contains("created by a newer kubernix"), "{}", err);
        assert!(err.contains("remove the run directory"), "{}", err);
        assert_eq!(read_to_string(&file)?, content);
        Ok(())
    }

    #[test]
    fn check_version_bounds() -> Result<()> {
        let c = test_config()?;
        let err = c.check_version(u32::MAX).unwrap_err().to_string();
        assert!(err.contains("newer kubernix"), "{}", err);
        assert!(!c.check_version(Config::VERSION)?);
        assert_eq!(
            c.check_version(Config::OLDEST_VERSION)?,
            Config::OLDEST_VERSION < Config::VERSION
        );
        Ok(())
    }

    #[test]
    fn packages_from_env_comma_separated() {
        temp_env::with_var("KUBERNIX_PACKAGES", Some("kubernetes-helm,jq"), || {
            let c = Config::parse_from(["kubernix"]);
            assert_eq!(c.packages(), ["kubernetes-helm", "jq"]);
        });
    }

    #[test]
    fn packages_from_args() {
        temp_env::with_var_unset("KUBERNIX_PACKAGES", || {
            for args in [
                &["kubernix", "-p", "helm", "jq"][..],
                &["kubernix", "-p", "helm", "-p", "jq"],
                &["kubernix", "--packages=helm,jq"],
            ] {
                assert_eq!(Config::parse_from(args).packages(), ["helm", "jq"]);
            }
        });
    }

    #[test]
    fn addons_from_env_comma_separated() -> Result<()> {
        temp_env::with_var("KUBERNIX_ADDONS", Some("coredns,coredns"), || {
            assert_eq!(parse(&[])?.addons(), [Addon::Coredns, Addon::Coredns]);
            Ok::<_, anyhow::Error>(())
        })?;
        temp_env::with_var("KUBERNIX_ADDONS", Some("none"), || {
            assert!(parse(&[])?.addons().is_empty());
            Ok(())
        })
    }

    #[test]
    fn addons_from_args() -> Result<()> {
        temp_env::with_var_unset("KUBERNIX_ADDONS", || {
            let c = parse(&["-a", "coredns", "-a", "coredns"])?;
            assert_eq!(c.addons(), [Addon::Coredns, Addon::Coredns]);
            assert_eq!(parse(&[])?.addons(), [Addon::Coredns]);
            assert_eq!(parse(&["--addons=coredns"])?.addons(), [Addon::Coredns]);
            assert!(parse(&["-a"])?.addons().is_empty());
            assert!(parse(&["--addons=none"])?.addons().is_empty());
            assert!(parse(&["--addons=coredns,invalid"]).is_err());
            let err = parse(&["--addons=coredns,none"])
                .err()
                .context("expected error")?
                .to_string();
            assert!(err.contains("cannot be combined"), "{}", err);
            Ok(())
        })
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
        assert!(!toml::to_string(&c)?.contains("rootless"));
        c.set_rootless(true);
        assert!(toml::to_string(&c)?.contains("rootless = true"));
        Ok(())
    }

    #[test]
    fn try_load_file_preserves_rootless() -> Result<()> {
        let mut c = Config {
            root: tempdir()?.keep(),
            ..Config::parse_from([""; 0])
        };
        c.to_file()?;
        c.set_rootless(true);
        c.try_load_file()?;
        assert!(c.is_rootless());
        Ok(())
    }

    /// Parse the arguments like `from_args` and use a new root, which
    /// contains the configuration of a previous run with `stored_args`.
    fn existing(stored_args: &[&str], args: &[&str]) -> Result<Config> {
        let root = tempdir()?.keep();
        let parse = |args: &[&str]| -> Result<Config> {
            let matches = Config::command().try_get_matches_from(
                ["kubernix", "--root", root.to_str().context("invalid root")?]
                    .iter()
                    .chain(args),
            )?;
            Ok(Config::from_matches(&matches)?)
        };
        parse(stored_args)?.to_file()?;
        let mut c = parse(args)?;
        c.try_load_file()?;
        Ok(c)
    }

    #[test]
    fn from_matches_explicit() -> Result<()> {
        let matches = Config::command().try_get_matches_from(["kubernix", "--nodes=2"])?;
        let c = Config::from_matches(&matches)?;
        assert!(c.is_explicit("nodes"));
        assert!(!c.is_explicit("cidr"));
        assert!(!c.is_explicit("root"));
        Ok(())
    }

    #[test]
    fn from_matches_explicit_env() -> Result<()> {
        temp_env::with_var("KUBERNIX_LOG_LEVEL", Some("debug"), || -> Result<()> {
            let matches = Config::command().try_get_matches_from(["kubernix"])?;
            let c = Config::from_matches(&matches)?;
            assert!(c.is_explicit("log_level"));
            assert_eq!(c.log_level(), LevelFilter::Debug);
            Ok(())
        })
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
        // Serialized with the tests which set the variable
        temp_env::with_var_unset("KUBERNIX_PACKAGES", || {
            let stored = ["--packages=hello"];
            let c = existing(&stored, &[])?;
            assert_eq!(c.packages(), ["hello"]);

            let c = existing(&stored, &["--packages", "hello", "kubernetes-helm"])?;
            assert_eq!(c.packages(), ["hello", "kubernetes-helm"]);
            Ok(())
        })
    }

    #[test]
    fn merge_overrides_nix_environment_from_env() -> Result<()> {
        temp_env::with_vars(
            [
                ("KUBERNIX_PACKAGES", Some("kubernetes-helm,jq")),
                ("KUBERNIX_ADDONS", Some("coredns")),
            ],
            || -> Result<()> {
                let c = existing(&["--packages=hello", "--addons"], &[])?;
                assert_eq!(c.packages(), ["kubernetes-helm", "jq"]);
                assert_eq!(c.addons(), [Addon::Coredns]);
                Ok(())
            },
        )
    }

    #[test]
    fn merge_ignores_stored_root() -> Result<()> {
        let mut c = test_config()?;
        let root = c.root.clone();
        let mut stored = test_config()?;
        stored.root = PathBuf::from("relative");
        c.merge(stored)?;
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
        c.merge(stored)
    }
}
