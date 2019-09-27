use crate::{
    config::Config,
    kubeconfig::KubeConfig,
    process::{Process, Stoppable},
};
use failure::Fallible;
use log::info;
use std::fs::{self, create_dir_all};

pub struct Proxy {
    process: Process,
}

impl Proxy {
    pub fn new(config: &Config, kubeconfig: &KubeConfig) -> Fallible<Proxy> {
        info!("Starting Proxy");

        let dir = config.root.join("proxy");
        create_dir_all(&dir)?;

        let yml = format!(
            r#"---
kind: KubeProxyConfiguration
apiVersion: kubeproxy.config.k8s.io/v1alpha1
clientConnection:
  kubeconfig: "{}"
mode: "iptables"
clusterCIDR: "{}"
"#,
            kubeconfig.proxy.display(),
            config.kube.cluster_cidr,
        );
        let yml_file = dir.join("config.yml");
        fs::write(&yml_file, yml)?;

        let mut process = Process::new(
            config,
            &[
                "kube-proxy".to_owned(),
                format!("--config={}", yml_file.display()),
            ],
        )?;

        process.wait_ready("Caches are synced")?;
        info!("Proxy is ready");
        Ok(Proxy { process })
    }
}

impl Stoppable for Proxy {
    fn stop(&mut self) {
        self.process.stop();
    }
}
