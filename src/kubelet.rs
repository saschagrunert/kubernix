use crate::{
    config::Config,
    container::Container,
    crio::Crio,
    kubeconfig::KubeConfig,
    network::Network,
    node::Node,
    pki::Pki,
    process::{Process, ProcessState, Stoppable},
};
use failure::{bail, format_err, Fallible};
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
        let node_name = Node::name(config, network, node);
        info!("Starting Kubelet ({})", node_name);
        const KUBELET: &str = "kubelet";

        let dir = config.root().join(KUBELET).join(&node_name);
        let root_dir = dir.join("run");
        if root_dir.display().to_string().len() + "kubelet.sock".len() > 100 {
            bail!(
                "Kubelet run path '{}' is too long for kubelet.sock",
                root_dir.display()
            )
        }

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

        let args = &[
            "--container-runtime=remote",
            &format!("--config={}", cfg.display()),
            &format!("--root-dir={}", root_dir.display()),
            &format!(
                "--container-runtime-endpoint={}",
                Crio::socket(config, network, node)?.to_socket_string(),
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
        ];

        let mut process = if config.nodes() > 1 {
            // Run inside a container
            let arg_hostname = &format!("--hostname-override={}", node_name);
            let mut modargs: Vec<&str> = vec![arg_hostname];
            modargs.extend(args);
            Container::exec(config, &dir, "Kubelet", KUBELET, &node_name, &modargs)?
        } else {
            // Run as usual process
            Process::start(&dir, "Kubelet", KUBELET, args)?
        };
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
