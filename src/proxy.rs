use crate::{config::Config, kubeconfig::KubeConfig, process::Process};
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
clusterCIDR: "10.200.0.0/16"
"#,
            kubeconfig.proxy.display(),
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

        process.wait_ready("Caches are synched")?;
        info!("Proxy is ready");
        Ok(Proxy { process })
    }

    pub fn stop(&mut self) -> Fallible<()> {
        self.process.stop()?;
        Ok(())
    }
}
