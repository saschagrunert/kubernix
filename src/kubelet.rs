use crate::{
    config::Config,
    kubeconfig::KubeConfig,
    pki::Pki,
    process::{Process, Stoppable},
};
use incdoc::incdoc;
use failure::Fallible;
use log::info;
use std::{
    fs::{self, create_dir_all},
    path::Path,
};

pub struct Kubelet {
    process: Process,
}

impl Kubelet {
    pub fn new(
        config: &Config,
        pki: &Pki,
        kubeconfig: &KubeConfig,
        socket: &Path,
    ) -> Fallible<Kubelet> {
        info!("Starting Kubelet");

        let dir = config.root.join("kubelet");
        create_dir_all(&dir)?;

        let yml = incdoc!(format!(
            r#"---
kind: KubeletConfiguration
apiVersion: kubelet.config.k8s.io/v1beta1
authentication:
  anonymous:
    enabled: false
  webhook:
    enabled: true
  x509:
    clientCAFile: "{}"
authorization:
  mode: Webhook
clusterDomain: "cluster.local"
clusterDNS:
  - "{}"
podCIDR: "{}"
runtimeRequestTimeout: "15m"
tlsCertFile: "{}"
tlsPrivateKeyFile: "{}"
"#,
            pki.ca.cert().display(),
            config.kube.cluster_dns,
            config.crio.cidr,
            pki.kubelet.cert().display(),
            pki.kubelet.key().display()
        ));
        let yml_file = dir.join("config.yml");
        fs::write(&yml_file, yml)?;

        let mut process = Process::new(
            config,
            &[
                "kubelet".to_owned(),
                format!("--config={}", yml_file.display()),
                "--container-runtime=remote".to_owned(),
                format!(
                    "--container-runtime-endpoint=unix://{}",
                    socket.display()
                ),
                format!("--kubeconfig={}", kubeconfig.kubelet.display()),
                "--image-pull-progress-deadline=2m".to_owned(),
                "--network-plugin=cni".to_owned(),
                "--register-node=true".to_owned(),
                "--v=2".to_owned(),
            ],
        )?;

        process.wait_ready("Successfully registered node")?;
        info!("Kubelet is ready");
        Ok(Kubelet { process })
    }
}

impl Stoppable for Kubelet {
    fn stop(&mut self) {
        self.process.stop();
    }
}
