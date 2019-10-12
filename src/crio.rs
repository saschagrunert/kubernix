use crate::{
    network::Network,
    node::Node,
    process::{Process, ProcessState, Stoppable},
    system::System,
    Config, RUNTIME_ENV,
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
    fmt::{Display, Formatter, Result},
    fs::{self, create_dir_all},
    path::PathBuf,
    process::Command,
    thread::sleep,
    time::{Duration, Instant},
};

pub struct Crio {
    process: Process,
    socket: CriSocket,
    node_name: String,
}

/// Simple CRI socket abstraction
pub struct CriSocket(PathBuf);

impl Display for CriSocket {
    fn fmt(&self, f: &mut Formatter<'_>) -> Result {
        write!(f, "{}", self.0.display())
    }
}

impl CriSocket {
    pub fn to_socket_string(&self) -> String {
        format!("unix://{}", self.0.display())
    }
}

impl Crio {
    pub fn start(config: &Config, node: u8, network: &Network) -> ProcessState {
        let node_name = Node::name(node);
        info!("Starting CRI-O ({})", node_name);

        let conmon = System::find_executable("conmon")?;
        let loopback = System::find_executable("loopback")?;
        let cni_plugin = loopback
            .parent()
            .ok_or_else(|| format_err!("Unable to find CNI plugin dir"))?;

        let dir = Self::path(config, node);
        let crio_config = dir.join("crio.conf");
        let policy_json = dir.join("policy.json");
        let cni = dir.join("cni");

        if !dir.exists() {
            create_dir_all(&dir)?;
            create_dir_all(&cni)?;

            let cidr = network
                .crio_cidrs()
                .get(node as usize)
                .ok_or_else(|| format_err!("Unable to find CIDR for {}", node_name))?;
            fs::write(
                cni.join("10-bridge.json"),
                to_string_pretty(&json!({
                    "cniVersion": "0.3.1",
                    "name": format!("kubernix-{}", node_name),
                    "type": "bridge",
                    "bridge": format!("{}.{}", Network::INTERFACE_PREFIX, node),
                    "isGateway": true,
                    "ipMasq": true,
                    "hairpinMode": true,
                    "ipam": {
                        "type": "host-local",
                        "routes": [{ "dst": "0.0.0.0/0" }],
                        "ranges": [[{ "subnet": cidr }]]
                    }
                }))?,
            )?;

            fs::write(
                &policy_json,
                to_string_pretty(&json!({
                    "default": [{ "type": "insecureAcceptAnything" }]
                }))?,
            )?;

            // Pseudo config to not load local configuration values
            fs::write(&crio_config, "")?;
        }
        let socket = Self::socket(config, node);

        let mut process = Process::start(
            &dir,
            "crio",
            &[
                &format!("--config={}", crio_config.display()),
                "--log-level=debug",
                &format!(
                    "--storage-driver={}",
                    if config.container() { "vfs" } else { "overlay" }
                ),
                &format!("--conmon={}", conmon.display()),
                &format!("--listen={}", socket),
                &format!("--root={}", dir.join("storage").display()),
                &format!("--runroot={}", dir.join("run").display()),
                &format!("--cni-config-dir={}", cni.display()),
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
        info!("CRI-O is ready ({})", node_name);
        Ok(Box::new(Crio {
            process,
            socket,
            node_name,
        }))
    }

    /// Retrieve the CRI socket
    pub fn socket(config: &Config, node: u8) -> CriSocket {
        CriSocket(Self::path(config, node).join("crio.sock"))
    }

    /// Retrieve the working path for the node
    fn path(config: &Config, node: u8) -> PathBuf {
        config.root().join("crio").join(Node::name(node))
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
                        debug!("Killing conmon process {} ({})", p.pid, self.node_name);
                        if let Err(e) = kill(Pid::from_raw(p.pid), Signal::SIGTERM) {
                            debug!("Unable to kill PID {}: {}", p.pid, e);
                        }
                        found = true;
                    }
                    if !found {
                        debug!("All conmon processes exited ({})", self.node_name);
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
        debug!("Removing all CRI-O workloads on {}", self.node_name);

        let output = Command::new("crictl")
            .env(RUNTIME_ENV, self.socket.to_socket_string())
            .arg("pods")
            .arg("-q")
            .output()?;
        let stdout = String::from_utf8(output.stdout)?;
        if !output.status.success() {
            debug!("critcl pods stdout ({}): {}", self.node_name, stdout);
            debug!(
                "critcl pods stderr ({}): {}",
                self.node_name,
                String::from_utf8(output.stderr)?
            );
            bail!("crictl pods command failed ({})", self.node_name);
        }

        for x in stdout.lines() {
            debug!("Removing pod {} on {}", x, self.node_name);
            let output = Command::new("crictl")
                .env(RUNTIME_ENV, self.socket.to_socket_string())
                .arg("rmp")
                .arg("-f")
                .arg(x)
                .output()?;
            if !output.status.success() {
                debug!(
                    "critcl rmp stdout ({}): {}",
                    self.node_name,
                    String::from_utf8(output.stdout)?
                );
                debug!(
                    "critcl rmp stderr ({}): {}",
                    self.node_name,
                    String::from_utf8(output.stderr)?
                );
                bail!("crictl rmp command failed ({})", self.node_name);
            }
        }

        debug!("All workloads removed on {}", self.node_name);
        Ok(())
    }
}

impl Stoppable for Crio {
    fn stop(&mut self) -> Fallible<()> {
        // Remove all running containers
        self.remove_all_containers().map_err(|e| {
            format_err!(
                "Unable to remove CRI-O containers on {}: {}",
                self.node_name,
                e
            )
        })?;

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
