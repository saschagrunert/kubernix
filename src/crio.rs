use crate::{config::Config, process::Process};
use failure::{format_err, Fallible};
use log::info;
use std::{
    env,
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
        let mut process = Process::new(
            config,
            &[
                "crio".to_owned(),
                format!("--log-level={}", config.log.level),
                format!("--conmon={}", conmon.display()),
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
