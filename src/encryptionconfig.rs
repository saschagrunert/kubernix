use crate::Config;
use base64::encode;
use failure::Fallible;
use log::info;
use rand::{thread_rng, Rng};
use std::{
    fs::{self, create_dir_all},
    path::{Path, PathBuf},
};

pub struct EncryptionConfig {
    path: PathBuf,
}

impl EncryptionConfig {
    pub fn new(config: &Config) -> Fallible<EncryptionConfig> {
        info!("Creating encryption config");

        let rnd = thread_rng().gen::<[u8; 32]>();
        let b64 = encode(&rnd);
        let yml = format!(include_str!("assets/encryptionconfig.yml"), b64);

        let encryption_dir = &config.root().join("encryption");
        create_dir_all(encryption_dir)?;

        let path = encryption_dir.join("config.yml");
        fs::write(&path, yml)?;
        Ok(EncryptionConfig { path })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::tests::{test_config, test_config_wrong_root};

    #[test]
    fn encryptionconfig_success() -> Fallible<()> {
        let c = test_config()?;
        let e = EncryptionConfig::new(&c)?;
        assert!(e.path().exists());
        Ok(())
    }

    #[test]
    fn encryptionconfig_failure() -> Fallible<()> {
        let c = test_config_wrong_root()?;
        assert!(EncryptionConfig::new(&c).is_err());
        Ok(())
    }
}
