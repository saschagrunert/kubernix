use crate::{
    process::{Process, Startable, Stoppable},
    Config, Kubernix, CRIO_DIR, RUNTIME_ENV,
};
use failure::{bail, format_err, Fallible};
use log::{debug, info};
use nix::{
    mount::{umount2, MntFlags},
    sys::signal::{kill, Signal},
    unistd::Pid,
};
use psutil::process;
use serde_json::{json, to_string_pretty};
use std::{
    fs::{self, create_dir_all},
    path::{Path, PathBuf},
    process::Command,
    thread::sleep,
    time::Duration,
};
use walkdir::WalkDir;

pub struct Crio {
    process: Process,
    socket: PathBuf,
}

impl Crio {
    pub fn start(config: &Config, socket: &Path) -> Fallible<Startable> {
        info!("Starting CRI-O");
        let conmon = Kubernix::find_executable("conmon")?;
        let bridge = Kubernix::find_executable("bridge")?;
        let cni = bridge
            .parent()
            .ok_or_else(|| format_err!("Unable to find CNI plugin dir"))?;

        let dir = config.root().join(CRIO_DIR);
        create_dir_all(&dir)?;

        let cni_config = dir.join("cni");
        create_dir_all(&cni_config)?;
        let bridge_json = cni_config.join("bridge.json");
        fs::write(
            bridge_json,
            to_string_pretty(&json!({
              "cniVersion": "0.3.1",
              "name": "crio-bridge",
              "type": "bridge",
              "bridge": "kubernix1",
              "isGateway": true,
              "ipMasq": true,
              "hairpinMode": true,
              "ipam": {
                "type": "host-local",
                "routes": [{ "dst": "0.0.0.0/0" }],
                "ranges": [[{ "subnet": config.crio_cidr() }]]
              }
            }))?,
        )?;

        let policy_json = dir.join("policy.json");
        fs::write(
            &policy_json,
            to_string_pretty(&json!({
              "default": [{
                  "type": "insecureAcceptAnything"
              }]
            }))?,
        )?;

        let run_root = dir.join("run");
        let storage_driver = "overlay";
        let storage_root = dir.join("storage");

        let mut process = Process::start_with_dead_closure(
            config,
            "crio",
            &[
                "--log-level=debug",
                &format!("--storage-driver={}", storage_driver),
                &format!("--conmon={}", conmon.display()),
                &format!("--listen={}", socket.display()),
                &format!("--root={}", storage_root.display()),
                &format!("--runroot={}", run_root.display()),
                &format!("--cni-config-dir={}", cni_config.display()),
                &format!("--cni-plugin-dir={}", cni.display()),
                "--registry=docker.io",
                &format!("--signature-policy={}", policy_json.display()),
                &format!(
                    "--runtimes=local-runc:{}:{}",
                    Kubernix::find_executable("runc")?.display(),
                    dir.join("runc").display()
                ),
                "--default-runtime=local-runc",
            ],
            move || Self::cleanup(&run_root, &storage_root, &storage_driver),
        )?;

        process.wait_ready("sandboxes:")?;
        info!("CRI-O is ready");
        Ok(Box::new(Crio {
            process,
            socket: socket.to_path_buf(),
        }))
    }

    /// Try to cleanup a dead CRI-O process
    fn cleanup(run_root: &Path, storage_root: &Path, storage_driver: &str) {
        // Remove all conmon processes
        loop {
            match process::all() {
                Err(e) => {
                    debug!("Unable to retrieve processes: {}", e);
                    sleep(Duration::from_millis(100));
                }
                Ok(procs) => {
                    let mut found = false;
                    for p in procs.iter().filter(|p| &p.comm == "conmon") {
                        debug!("Killing conmon process {}", p.pid);
                        if let Err(e) = kill(Pid::from_raw(p.pid), Signal::SIGTERM) {
                            debug!("Unable to kill PID {}: {}", p.pid, e);
                        }
                        found = true;
                    }
                    if !found {
                        debug!("All conmon processes exited");
                        break;
                    }
                    sleep(Duration::from_millis(100));
                }
            }
        }

        // Umount every shared memory (SHM)
        for entry in WalkDir::new(run_root)
            .into_iter()
            .filter_map(|e| e.ok())
            .filter(|x| {
                x.path()
                    .to_str()
                    .map(|e| e.contains("shm"))
                    .unwrap_or(false)
            })
        {
            debug!("Umounting: {}", entry.path().display());
            if let Err(e) = umount2(entry.path(), MntFlags::MNT_FORCE) {
                debug!("Unable to umount '{}': {}", entry.path().display(), e)
            }
        }

        // Umount the storage dir
        let storage = storage_root.join(storage_driver);
        if let Err(e) = umount2(&storage, MntFlags::MNT_FORCE) {
            debug!("Unable to umount '{}': {}", storage.display(), e);
        }
    }

    /// Remove all containers via crictl invocations
    fn remove_all_containers(&self) -> Fallible<()> {
        debug!("Removing all CRI-O workloads");
        let env_value = format!("unix://{}", self.socket.display());

        let output = Command::new("crictl")
            .env(RUNTIME_ENV, &env_value)
            .arg("pods")
            .arg("-q")
            .output()?;
        let stdout = String::from_utf8(output.stdout)?;
        if !output.status.success() {
            debug!("critcl stdout: {}", stdout);
            debug!("critcl stderr: {}", String::from_utf8(output.stderr)?);
            bail!("crictl pods command failed");
        }

        for x in stdout.lines() {
            debug!("Removing pod {}", x);
            let output = Command::new("crictl")
                .env(RUNTIME_ENV, &env_value)
                .arg("rmp")
                .arg("-f")
                .arg(x)
                .output()?;
            if !output.status.success() {
                debug!("critcl stdout: {}", String::from_utf8(output.stdout)?);
                debug!("critcl stderr: {}", String::from_utf8(output.stderr)?);
                bail!("crictl rmp command failed");
            }
        }

        debug!("All workloads removed");
        Ok(())
    }
}

impl Stoppable for Crio {
    fn stop(&mut self) -> Fallible<()> {
        // Remove all running containers
        self.remove_all_containers()?;

        // Stop the process
        self.process.stop()
    }
}
