//! Configuration related structures
use crate::{podman::Podman, system::System};
use anyhow::{Context, Result, bail};
use clap::{
    ArgMatches, CommandFactory, FromArgMatches, Parser, Subcommand as ClapSubcommand, ValueEnum,
    builder::styling, parser::ValueSource,
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
///
/// ```
/// use kubernix::LogFormat;
///
/// assert_eq!(LogFormat::Text.to_string(), "text");
/// assert_eq!(LogFormat::Json.to_string(), "json");
/// ```
#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq, Serialize, ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum LogFormat {
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
///
/// ```
/// use kubernix::CriRuntime;
///
/// assert_eq!(CriRuntime::Crio.to_string(), "crio");
/// assert_eq!(CriRuntime::Containerd.to_string(), "containerd");
/// ```
#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq, Serialize, ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum CriRuntime {
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
///
/// ```
/// use kubernix::OciRuntime;
///
/// assert_eq!(OciRuntime::Crun.to_string(), "crun");
/// assert_eq!(OciRuntime::Runc.to_string(), "runc");
/// ```
#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq, Serialize, ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum OciRuntime {
    /// crun OCI runtime (default)
    #[default]
    Crun,
    /// runc OCI runtime
    Runc,
}

impl OciRuntime {
    /// All supported OCI runtimes, registered as CRI runtime handlers.
    pub const ALL: [OciRuntime; 2] = [OciRuntime::Crun, OciRuntime::Runc];

    /// The runtime handler and executable name.
    pub fn name(self) -> &'static str {
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

#[derive(Clone, Parser, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
#[command(
    after_help = "More info at: https://github.com/saschagrunert/kubernix",
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
        long = "log-level",
        short = 'l',
        value_name = "LEVEL"
    )]
    /// The logging level of the application
    log_level: LevelFilter,

    #[arg(
        default_value = "text",
        env = "KUBERNIX_LOG_FORMAT",
        long = "log-format",
        short = 'f',
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
        value_name = "PACKAGE",
    )]
    /// Additional dependencies to be added to the environment
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
    /// The number of nodes to be registered
    nodes: u8,

    #[arg(
        env = "KUBERNIX_CONTAINER_RUNTIME",
        long = "container-runtime",
        default_value = Podman::EXECUTABLE,
        requires = "nodes",
        short = 'u',
        value_name = "RUNTIME",
    )]
    /// The container runtime to be used for the nodes, irrelevant if `nodes` equals to `1`
    container_runtime: String,

    #[arg(
        conflicts_with = "shell",
        env = "KUBERNIX_NO_SHELL",
        long = "no-shell",
        short = 'e'
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
        value_name = "CRI_RUNTIME"
    )]
    /// The CRI runtime to use (crio or containerd)
    cri_runtime: CriRuntime,

    #[serde(default)]
    #[arg(
        default_value = "crun",
        env = "KUBERNIX_OCI_RUNTIME",
        long = "oci-runtime",
        value_name = "OCI_RUNTIME"
    )]
    /// The default OCI runtime (crun or runc), both are registered as runtime handlers
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
        value_name = "ADDON",
        value_parser = Config::validate_addon,
    )]
    /// Cluster addons to deploy (available: coredns)
    addons: Vec<String>,
}

impl Config {
    /// The active subcommand, if any (e.g. `shell`).
    pub fn subcommand(&self) -> &Option<SubCommand> {
        &self.subcommand
    }

    /// Directory where all runtime data is stored.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Configured log verbosity level.
    pub fn log_level(&self) -> LevelFilter {
        self.log_level
    }

    /// Output format for log messages (text or JSON).
    pub fn log_format(&self) -> LogFormat {
        self.log_format
    }

    /// Parent CIDR from which cluster, service, and pod subnets are carved.
    pub fn cidr(&self) -> Ipv4Network {
        self.cidr
    }

    /// Custom Nix package overlay path, if provided via `--overlay`.
    pub fn overlay(&self) -> Option<&Path> {
        self.overlay.as_deref()
    }

    /// Additional Nix packages to include in the runtime environment.
    pub fn packages(&self) -> &[String] {
        &self.packages
    }

    /// Interactive shell executable (defaults to `$SHELL`, fallback `sh`).
    pub fn shell(&self) -> Option<&str> {
        self.shell.as_deref()
    }

    /// Number of cluster nodes to register.
    pub fn nodes(&self) -> u8 {
        self.nodes
    }

    /// Container runtime executable used for multi-node setups.
    pub fn container_runtime(&self) -> &str {
        &self.container_runtime
    }

    /// Whether to skip spawning an interactive shell after bootstrap.
    pub fn no_shell(&self) -> bool {
        self.no_shell
    }

    /// Custom Dockerfile path for multi-node container image builds.
    pub fn dockerfile(&self) -> Option<&Path> {
        self.dockerfile.as_deref()
    }

    /// CRI runtime implementation (CRI-O or containerd).
    pub fn cri_runtime(&self) -> CriRuntime {
        self.cri_runtime
    }

    /// Default OCI runtime handler (crun or runc).
    pub fn oci_runtime(&self) -> OciRuntime {
        self.oci_runtime
    }

    /// Cluster addons to deploy after bootstrap (e.g. `coredns`).
    pub fn addons(&self) -> &[String] {
        &self.addons
    }

    /// Whether the cluster is running in rootless (user namespace) mode.
    pub fn is_rootless(&self) -> bool {
        self.rootless.unwrap_or_default()
    }

    /// Set rootless mode (auto-detected from UID at startup).
    pub fn set_rootless(&mut self, val: bool) {
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

impl Config {
    const FILENAME: &'static str = "kubernix.toml";

    /// Make the configs root path absolute
    pub fn canonicalize_root(&mut self) -> Result<()> {
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
    pub fn to_file(&self) -> Result<()> {
        self.create_root_dir()?;
        fs::write(self.root().join(Self::FILENAME), toml::to_string(&self)?)
            .context("Unable to write configuration to file")?;
        Ok(())
    }

    /// Returns true if the configuration file exists in the root path.
    pub fn file_exists(&self) -> bool {
        self.root().join(Self::FILENAME).exists()
    }

    /// Read the configuration from the internal set root path and merge
    /// it into the current one. If not existing, write the current
    /// configuration to the path.
    pub fn try_load_file(&mut self) -> Result<()> {
        let file = self.root().join(Self::FILENAME);
        if file.exists() {
            let stored = toml::from_str(&read_to_string(&file).with_context(|| {
                format!(
                    "Unable to read expected configuration file '{}'",
                    file.display(),
                )
            })?)
            .with_context(|| format!("Unable to load config file '{}'", file.display()))?;
            self.merge(stored)?;
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
    pub fn validate(&self) -> Result<()> {
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
    pub fn shell_ok(&self) -> Result<String> {
        let shell = self.shell.as_ref().context("No shell set")?;
        Ok(shell.into())
    }

    /// Returns true if multi node support is enabled
    pub fn multi_node(&self) -> bool {
        self.nodes() > 1
    }

    const VALID_ADDONS: &'static [&'static str] = &["coredns"];

    fn validate_addon(s: &str) -> std::result::Result<String, String> {
        if Self::VALID_ADDONS.contains(&s) {
            Ok(s.to_string())
        } else {
            Err(format!(
                "unknown addon '{}', valid addons: {}",
                s,
                Self::VALID_ADDONS.join(", ")
            ))
        }
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

    #[test]
    fn validate_addon_known() {
        assert!(Config::validate_addon("coredns").is_ok());
    }

    #[test]
    fn validate_addon_unknown() {
        let result = Config::validate_addon("invalid");
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("unknown addon"));
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
        let stored = ["--packages=hello"];
        let c = existing(&stored, &[])?;
        assert_eq!(c.packages(), ["hello"]);

        let c = existing(&stored, &["--packages", "hello", "kubernetes-helm"])?;
        assert_eq!(c.packages(), ["hello", "kubernetes-helm"]);
        Ok(())
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
