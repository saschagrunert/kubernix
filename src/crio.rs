use crate::{
    network::Network,
    process::{Process, Startable, Stoppable},
    system::System,
    Config, CRIO_DIR, RUNTIME_ENV,
};
use failure::{bail, format_err, Fallible};
use log::{debug, info};
use nix::{
    sys::signal::{kill, Signal},
    unistd::Pid,
};
use psutil::process;
use serde_json::{json, to_string_pretty};
use std::{
    fs::{self, create_dir_all},
    process::Command,
    thread::sleep,
    time::{Duration, Instant},
};

pub struct Crio {
    process: Process,
    socket: String,
}

impl Crio {
    pub fn start(config: &Config, network: &Network) -> Fallible<Startable> {
        info!("Starting CRI-O");
        let conmon = System::find_executable("conmon")?;
        let loopback = System::find_executable("loopback")?;
        let cni_plugin = loopback
            .parent()
            .ok_or_else(|| format_err!("Unable to find CNI plugin dir"))?;

        let dir = config.root().join(CRIO_DIR);
        create_dir_all(&dir)?;

        let cni_config = dir.join("cni");
        create_dir_all(&cni_config)?;
        fs::write(
            cni_config.join("bridge.json"),
            to_string_pretty(&json!({
              "cniVersion": "0.3.1",
              "name": "crio-kubernix",
              "type": "bridge",
              "bridge": Network::BRIDGE,
              "isGateway": true,
              "ipMasq": true,
              "hairpinMode": true,
              "ipam": {
                "type": "host-local",
                "routes": [{ "dst": "0.0.0.0/0" }],
                "ranges": [[{ "subnet": network.crio_cidr() }]]
              }
            }))?,
        )?;
        fs::write(
            cni_config.join("loopback.json"),
            to_string_pretty(&json!({
              "cniVersion": "0.3.1",
              "type": "loopback",
            }))?,
        )?;

        let policy_json = dir.join("policy.json");
        fs::write(
            &policy_json,
            to_string_pretty(&json!({
              "default": [{ "type": "insecureAcceptAnything" }]
            }))?,
        )?;

        // Pseudo config to not load local configuration values
        let crio_config = dir.join("crio.conf");
        fs::write(&crio_config, "")?;

        let mut process = Process::start(
            config,
            &dir,
            "crio",
            &[
                &format!("--config={}", crio_config.display()),
                "--log-level=debug",
                "--storage-driver=overlay",
                &format!("--conmon={}", conmon.display()),
                &format!("--listen={}", network.crio_socket()),
                &format!("--root={}", dir.join("storage").display()),
                &format!("--runroot={}", dir.join("run").display()),
                &format!("--cni-config-dir={}", cni_config.display()),
                &format!("--cni-plugin-dir={}", cni_plugin.display()),
                "--registry=docker.io",
                &format!("--signature-policy={}", policy_json.display()),
                &format!(
                    "--runtimes=local-runc:{}:{}",
                    System::find_executable("runc")?.display(),
                    dir.join("runc").display()
                ),
                "--default-runtime=local-runc",
            ],
        )?;

        process.wait_ready("sandboxes:")?;
        info!("CRI-O is ready");
        Ok(Box::new(Crio {
            process,
            socket: network.crio_socket().to_socket_string(),
        }))
    }

    /// Try to cleanup all related resources
    fn stop_conmons(&self) {
        // Remove all conmon processes
        let now = Instant::now();
        while now.elapsed().as_secs() < 5 {
            match process::all() {
                Err(e) => {
                    debug!("Unable to retrieve processes: {}", e);
                    sleep(Duration::from_secs(1));
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
                    // Give the signal time to arrive
                    sleep(Duration::from_millis(100));
                }
            }
        }
    }

    /// Remove all containers via crictl invocations
    fn remove_all_containers(&self) -> Fallible<()> {
        debug!("Removing all CRI-O workloads");

        let output = Command::new("crictl")
            .env(RUNTIME_ENV, &self.socket)
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
                .env(RUNTIME_ENV, &self.socket)
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
        self.remove_all_containers()
            .map_err(|e| format_err!("Unable to remove CRI-O containers: {}", e))?;

        // Stop the process, should never really fail
        self.process.stop()
    }
}

impl Drop for Crio {
    fn drop(&mut self) {
        // Remove conmon processes
        self.stop_conmons();
    }
}
