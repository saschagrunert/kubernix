//! Kubernetes encryption-at-rest configuration.
//!
//! Generates a random secretbox key and writes the EncryptionConfiguration
//! manifest consumed by the API server's `--encryption-provider-config` flag.

use crate::{Config, etcd::Etcd, files::open_nofollow, write_private};
use anyhow::{Context, Result, bail};
use base64::{Engine, engine::general_purpose::STANDARD};
use log::{debug, info};
use std::{
    fs::create_dir_all,
    io::Read,
    path::{Path, PathBuf},
};

/// Encryption-at-rest configuration file for the API server.
#[must_use]
pub struct EncryptionConfig {
    path: PathBuf,
}

impl EncryptionConfig {
    /// Path to the generated EncryptionConfiguration YAML file.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Generate the encryption config with a random secretbox key, or
    /// reuse an existing one to support cluster restarts. A new key is
    /// never generated if etcd contains data, since the secrets stored in
    /// it would become unreadable.
    pub fn new(config: &Config) -> Result<EncryptionConfig> {
        let dir = &config.root().join("encryptionconfig");
        create_dir_all(dir).context("Unable to create encryption config directory")?;
        let path = dir.join("config.yml");

        if let Err(e) = Self::check(&path) {
            if Etcd::has_data(config) {
                bail!(
                    "The encryption config '{}' is missing or invalid ({:#}), but the secrets \
                     in the etcd data '{}' are encrypted with its key. Restore it or remove the \
                     run directory",
                    path.display(),
                    e,
                    Etcd::data_dir(config).display(),
                )
            }
            debug!("Encryption config is unusable: {:#}", e);
            info!("Creating encryption config");
            let mut rnd = [0u8; 32];
            getrandom::fill(&mut rnd).context("Unable to generate encryption key")?;
            let b64 = STANDARD.encode(rnd);
            let yml = format!(include_str!("assets/encryptionconfig.yml"), secret = b64);
            // Only the owner can read the key
            write_private(&path, yml.as_bytes()).context("Unable to write encryption config")?;
        }

        Ok(EncryptionConfig { path })
    }

    /// Check that the config at `path` contains a valid key.
    fn check(path: &Path) -> Result<()> {
        let mut content = String::new();
        open_nofollow(path)
            .and_then(|mut f| f.read_to_string(&mut content))
            .with_context(|| format!("Unable to read '{}'", path.display()))?;
        let secret = content
            .lines()
            .find_map(|l| l.trim().strip_prefix("secret: "))
            .context("no secret")?;
        if STANDARD.decode(secret).context("invalid secret")?.len() != 32 {
            bail!("invalid secret length")
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::tests::{test_config, test_config_wrong_root};
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn encryptionconfig_success() -> Result<()> {
        let c = test_config()?;
        let e = EncryptionConfig::new(&c)?;
        let mode = std::fs::metadata(e.path())?.permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
        let content = std::fs::read_to_string(e.path())?;
        let secret = content
            .lines()
            .find_map(|l| l.trim().strip_prefix("secret: "))
            .context("missing secret")?;
        assert_eq!(STANDARD.decode(secret)?.len(), 32);
        Ok(())
    }

    #[test]
    fn encryptionconfig_reused() -> Result<()> {
        let c = test_config()?;
        let first = std::fs::read(EncryptionConfig::new(&c)?.path())?;
        let second = std::fs::read(EncryptionConfig::new(&c)?.path())?;
        assert_eq!(first, second);
        Ok(())
    }

    #[test]
    fn encryptionconfig_replaces_invalid() -> Result<()> {
        let c = test_config()?;
        let path = EncryptionConfig::new(&c)?.path().to_path_buf();
        // Left behind by a crash of an older version
        std::fs::write(&path, "")?;
        let _ = EncryptionConfig::new(&c)?;
        EncryptionConfig::check(&path)
    }

    #[test]
    fn encryptionconfig_refuses_new_key_with_etcd_data() -> Result<()> {
        let c = test_config()?;
        let path = EncryptionConfig::new(&c)?.path().to_path_buf();
        std::fs::create_dir_all(Etcd::data_dir(&c).join("member"))?;
        let content = std::fs::read(&path)?;
        let _ = EncryptionConfig::new(&c)?;
        assert_eq!(std::fs::read(&path)?, content);

        std::fs::remove_file(&path)?;
        let err = EncryptionConfig::new(&c).err().context("no error")?;
        assert!(err.to_string().contains("etcd data"), "{err}");
        assert!(!path.exists());
        Ok(())
    }

    #[test]
    fn encryptionconfig_failure() -> Result<()> {
        let c = test_config_wrong_root()?;
        assert!(EncryptionConfig::new(&c).is_err());
        Ok(())
    }
}
