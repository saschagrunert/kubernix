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

        const KUBELET: &str = "kubelet";
        let dir = config.root().join(KUBELET).join(&node_name);
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

        let (cmd, mut args_vec) = if config.nodes() > 1 {
            (
                config.container_runtime().to_owned(),
                vec![
                    "exec",
                    &node_name,
                    "nix",
                    "run",
                    "-f",
                    "/kubernix",
                    "-c",
                    KUBELET,
                    &format!("--hostname-override={}", node_name),
                ]
                .into_iter()
                .map(|x| x.to_owned())
                .collect()
            )
        } else {
            (KUBELET.to_owned(), vec![])
        };

        args_vec.extend(
            vec![
                "--container-runtime=remote",
                &format!("--config={}", cfg.display()),
                &format!("--root-dir={}", dir.join("run").display()),
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
            ]
            .into_iter()
            .map(|x| x.to_owned())
            .collect::<Vec<String>>(),
        );
        let args = args_vec.iter().map(|x| x.as_str()).collect::<Vec<&str>>();

        let mut process = Process::start(&dir, KUBELET, &cmd, &args)?;

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
