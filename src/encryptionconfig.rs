//! Kubernetes encryption-at-rest configuration.
//!
//! Generates a random secretbox key and writes the EncryptionConfiguration
//! manifest consumed by the API server's `--encryption-provider-config` flag.

use crate::Config;
use anyhow::{Context, Result};
use base64::{Engine, engine::general_purpose::STANDARD};
use log::info;
use std::{
    fs::{OpenOptions, create_dir_all},
    io::Write,
    os::unix::fs::OpenOptionsExt,
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
    /// reuse an existing one to support cluster restarts.
    pub fn new(config: &Config) -> Result<EncryptionConfig> {
        let dir = &config.root().join("encryptionconfig");
        create_dir_all(dir).context("Unable to create encryption config directory")?;
        let path = dir.join("config.yml");

        // Create only if not already existing to make cluster reuse work
        if !path.exists() {
            info!("Creating encryption config");
            let mut rnd = [0u8; 32];
            getrandom::fill(&mut rnd).context("Unable to generate encryption key")?;
            let b64 = STANDARD.encode(rnd);
            let yml = format!(include_str!("assets/encryptionconfig.yml"), secret = b64);
            // Create the file with restricted permissions, so that the key
            // is never readable by others.
            OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&path)
                .and_then(|mut f| f.write_all(yml.as_bytes()))
                .context("Unable to write encryption config")?;
        }

        Ok(EncryptionConfig { path })
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
    fn encryptionconfig_failure() -> Result<()> {
        let c = test_config_wrong_root()?;
        assert!(EncryptionConfig::new(&c).is_err());
        Ok(())
    }
}
