use crate::{
    config::Config,
    crio::Crio,
    kubeconfig::KubeConfig,
    network::Network,
    node::Node,
    pki::Pki,
    process::{Process, ProcessState, Stoppable},
};
use failure::{format_err, Fallible};
use log::info;
use std::fs::{self, create_dir_all};

pub struct Kubelet {
    process: Process,
}

impl Kubelet {
    pub fn start(
        config: &Config,
        node: u8,
        network: &Network,
        pki: &Pki,
        kubeconfig: &KubeConfig,
    ) -> ProcessState {
        let node_name = Node::name(node);
        info!("Starting Kubelet ({})", node_name);

        let dir = config.root().join("kubelet").join(&node_name);
        create_dir_all(&dir)?;

        let idendity = pki
            .kubelets()
            .get(node as usize)
            .ok_or_else(|| format_err!("Unable to retrieve kubelet idendity for {}", node_name))?;

        let yml = format!(
            include_str!("assets/kubelet.yml"),
            ca = pki.ca().cert().display(),
            dns = network.dns()?,
            cidr = network
                .crio_cidrs()
                .get(node as usize)
                .ok_or_else(|| format_err!("Unable to retrieve kubelet CIDR"))?,
            cert = idendity.cert().display(),
            key = idendity.key().display(),
            port = 11250 + u16::from(node),
            healthzPort = 12250 + u16::from(node),
        );
        let cfg = dir.join("config.yml");

        if !cfg.exists() {
            fs::write(&cfg, yml)?;
        }

        let mut process = Process::start(
            &dir,
            "kubelet",
            config.container_runtime(),
            &[
                "exec",
                &node_name,
                "nix",
                "run",
                "-f",
                "/kubernix",
                "-c",
                "kubelet",
                &format!("--config={}", cfg.display()),
                &format!("--root-dir={}", dir.join("run").display()),
                &format!("--hostname-override=node-{}", node),
                "--container-runtime=remote",
                &format!(
                    "--container-runtime-endpoint={}",
                    Crio::socket(config, node).to_socket_string(),
                ),
                &format!(
                    "--kubeconfig={}",
                    kubeconfig
                        .kubelets()
                        .get(node as usize)
                        .ok_or_else(|| format_err!(
                            "Unable to retrieve kubelet config for {}",
                            node_name
                        ))?
                        .display()
                ),
                "--v=2",
            ],
        )?;

        process.wait_ready("Successfully registered node")?;
        info!("Kubelet is ready ({})", node_name);
        Ok(Box::new(Self { process }))
    }
}

impl Stoppable for Kubelet {
    fn stop(&mut self) -> Fallible<()> {
        self.process.stop()
    }
}
