use crate::Config;
use anyhow::Result;
use base64::{Engine, engine::general_purpose::STANDARD};
use log::info;
use rand::random;
use std::{
    fs::{self, create_dir_all},
    path::{Path, PathBuf},
};

pub struct EncryptionConfig {
    path: PathBuf,
}

impl EncryptionConfig {
    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn new(config: &Config) -> Result<EncryptionConfig> {
        let dir = &config.root().join("encryptionconfig");
        create_dir_all(dir)?;
        let path = dir.join("config.yml");

        // Create only if not already existing to make cluster reuse work
        if !path.exists() {
            info!("Creating encryption config");
            let rnd: [u8; 32] = random();
            let b64 = STANDARD.encode(rnd);
            let yml = format!(include_str!("assets/encryptionconfig.yml"), b64);
            fs::write(&path, yml)?;
        }

        Ok(EncryptionConfig { path })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::tests::{test_config, test_config_wrong_root};

    #[test]
    fn encryptionconfig_success() -> Result<()> {
        let c = test_config()?;
        let e = EncryptionConfig::new(&c)?;
        assert!(e.path().exists());
        Ok(())
    }

    #[test]
    fn encryptionconfig_failure() -> Result<()> {
        let c = test_config_wrong_root()?;
        assert!(EncryptionConfig::new(&c).is_err());
        Ok(())
    }
}
