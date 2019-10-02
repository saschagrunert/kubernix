use crate::Config;
use base64::encode;
use failure::Fallible;
use log::info;
use rand::{thread_rng, Rng};
use std::{
    fs,
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
        let path = config.root().join("encryption-config.yml");
        fs::write(&path, yml)?;
        Ok(EncryptionConfig { path })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}
