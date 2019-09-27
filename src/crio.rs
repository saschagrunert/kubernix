use crate::{
    process::{Process, Stoppable},
    Config, ASSETS_DIR,
};
use failure::{format_err, Fallible};
use log::info;
use std::{
    env,
    fs::{self, create_dir_all},
    path::{Path, PathBuf},
};

pub struct Crio {
    process: Process,
}

impl Crio {
    pub fn new(config: &Config, socket: &Path) -> Fallible<Crio> {
        info!("Starting CRI-O");
        let conmon = Self::find_executable("conmon")
            .ok_or_else(|| format_err!("Unable to find conmon in $PATH"))?;

        let bridge = Self::find_executable("bridge")
            .ok_or_else(|| format_err!("Unable to find CNI bridge in $PATH"))?;
        let cni = bridge
            .parent()
            .ok_or_else(|| format_err!("Unable to find CNI plugin dir"))?;

        let dir = config.root.join(&config.crio.dir);
        create_dir_all(&dir)?;

        let cni_config = dir.join("cni");
        create_dir_all(&cni_config)?;
        let bridge_json = cni_config.join("bridge.json");
        fs::write(
            bridge_json,
            format!(
                r#"{{
  "cniVersion": "0.3.1",
  "name": "crio-bridge",
  "type": "bridge",
  "bridge": "cni0",
  "isGateway": true,
  "ipMasq": true,
  "hairpinMode": true,
  "ipam": {{
    "type": "host-local",
    "routes": [{{ "dst": "0.0.0.0/0" }}],
    "ranges": [[{{ "subnet": "{}" }}]]
  }}
}}
"#,
                config.crio.cidr
            ),
        )?;

        let mut process = Process::new(
            config,
            &[
                "crio".to_owned(),
                "--log-level=debug".to_owned(),
                "--storage-driver=overlay".to_owned(),
                format!("--conmon={}", conmon.display()),
                format!("--listen={}", &socket.display()),
                format!("--root={}", dir.join("storage").display()),
                format!("--runroot={}", dir.join("run").display()),
                format!("--cni-config-dir={}", cni_config.display()),
                format!("--cni-plugin-dir={}", cni.display()),
                "--registry=docker.io".to_owned(),
                format!(
                    "--signature-policy={}",
                    Path::new(ASSETS_DIR).join("policy.json").display()
                ),
            ],
        )?;

        process.wait_ready("sandboxes:")?;
        info!("CRI-O is ready");
        Ok(Crio { process })
    }

    fn find_executable<P>(name: P) -> Option<PathBuf>
    where
        P: AsRef<Path>,
    {
        env::var_os("PATH").and_then(|paths| {
            env::split_paths(&paths)
                .filter_map(|dir| {
                    let full_path = dir.join(&name);
                    if full_path.is_file() {
                        Some(full_path)
                    } else {
                        None
                    }
                })
                .next()
        })
    }
}

impl Stoppable for Crio {
    fn stop(&mut self) {
        self.process.stop();
    }
}
