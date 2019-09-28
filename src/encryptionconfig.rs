use crate::Config;
use base64::encode;
use failure::Fallible;
use incdoc::incdoc;
use log::info;
use rand::{thread_rng, Rng};
use std::{fs, path::PathBuf};

pub struct EncryptionConfig {
    pub path: PathBuf,
}

impl EncryptionConfig {
    pub fn new(config: &Config) -> Fallible<EncryptionConfig> {
        info!("Creating encryptionconfig");

        let rnd = thread_rng().gen::<[u8; 32]>();
        let b64 = encode(&rnd);
        let yml = incdoc!(format!(
            "---
kind: EncryptionConfig
apiVersion: v1
resources:
  - resources:
      - secrets
    providers:
      - aescbc:
          keys:
            - name: key1
              secret: {}
      - identity: {{}}",
            b64
        ));
        let config = &config.root.join("encryption-config.yml");
        fs::write(config, yml)?;
        Ok(EncryptionConfig {
            path: config.clone(),
        })
    }
}
