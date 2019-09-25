use crate::{process::Process, Config, ASSETS_DIR};
use failure::{format_err, Fallible};
use log::info;
use std::{
    env,
    fs::{copy, create_dir_all},
    path::{Path, PathBuf},
};

pub struct Crio {
    process: Process,
}

impl Crio {
    pub fn new(config: &Config) -> Fallible<Crio> {
        info!("Starting CRI-O");
        let conmon = Self::find_executable("conmon")
            .ok_or_else(|| format_err!("Unable to find conmon in $PATH"))?;

        let bridge = Self::find_executable("bridge")
            .ok_or_else(|| format_err!("Unable to find CNI bridge in $PATH"))?;
        let cni = bridge
            .parent()
            .ok_or_else(|| format_err!("Unable to find CNI plugin dir"))?;

        let dir = config.root.join("crio");
        create_dir_all(&dir)?;

        let cni_config = dir.join("cni");
        create_dir_all(&cni_config)?;
        let bridge_json = Path::new("bridge.json");
        let cni_asset = Path::new(ASSETS_DIR).join(&bridge_json);
        copy(cni_asset, cni_config.join(&bridge_json))?;

        let mut process = Process::new(
            config,
            &[
                "crio".to_owned(),
                "--log-level=debug".to_owned(),
                "--storage-driver=overlay".to_owned(),
                format!("--conmon={}", conmon.display()),
                format!("--listen={}", dir.join("crio.sock").display()),
                format!("--root={}", dir.join("storage").display()),
                format!("--runroot={}", dir.join("run").display()),
                format!("--cni-config-dir={}", cni_config.display()),
                format!("--cni-plugin-dir={}", cni.display()),
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

    pub fn stop(&mut self) -> Fallible<()> {
        self.process.stop()?;
        Ok(())
    }
}
