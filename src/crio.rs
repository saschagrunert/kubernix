use crate::{
    network::Network,
    node::Node,
    process::{Process, ProcessState, Stoppable},
    system::System,
    Config, RUNTIME_ENV,
};
use failure::{bail, format_err, Fallible};
use log::{debug, info};
use serde_json::{json, to_string_pretty};
use std::{
    fmt::{Display, Formatter, Result},
    fs::{self, create_dir_all},
    path::PathBuf,
    process::Command,
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

const CRIO: &str = "crio";

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

            // Pseudo config to not load local configuration values
            fs::write(&crio_config, "")?;
            fs::write(&policy_json, include_str!("assets/policy.json"))?;
        }
        let socket = Self::socket(config, node);

        let (mut args_vec, cmd) = if config.nodes() > 1 {
            (
                vec![
                    "run",
                    "--rm",
                    "--net=host",
                    "--privileged",
                    &format!(
                        "--storage-driver={}",
                        if config.container() { "vfs" } else { "" }
                    ),
                    &format!("--hostname={}", node_name),
                    &format!("--name={}", node_name),
                    &format!("-v={v}:{v}", v = config.root().display()),
                    "docker.io/saschagrunert/kubernix:base",
                    CRIO,
                    "--storage-driver=vfs",
                ]
                .into_iter()
                .map(|x| x.to_owned())
                .collect(),
                config.container_runtime().to_owned(),
            )
        } else {
            (vec![], CRIO.to_owned())
        };

        args_vec.extend(
            vec![
                "--log-level=debug",
                "--registry=docker.io",
                &format!("--config={}", crio_config.display()),
                &format!("--conmon={}", conmon.display()),
                &format!("--listen={}", socket),
                &format!("--root={}", dir.join("storage").display()),
                &format!("--runroot={}", dir.join("run").display()),
                &format!("--cni-config-dir={}", cni.display()),
                &format!("--cni-plugin-dir={}", cni_plugin.display()),
                &format!("--signature-policy={}", policy_json.display()),
                &format!(
                    "--runtimes=local-runc:{}:{}",
                    System::find_executable("runc")?.display(),
                    dir.join("runc").display()
                ),
                "--default-runtime=local-runc",
            ]
            .into_iter()
            .map(|x| x.to_owned())
            .collect::<Vec<String>>(),
        );
        let args = args_vec.iter().map(|x| x.as_str()).collect::<Vec<&str>>();

        let mut process = Process::start(&dir, CRIO, &cmd, &args)?;

        process.wait_ready("sandboxes:")?;
        info!("CRI-O is ready ({})", node_name);
        Ok(Box::new(Self {
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
        config.root().join(CRIO).join(Node::name(node))
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
