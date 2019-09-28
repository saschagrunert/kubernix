use crate::{
    config::Config,
    encryptionconfig::EncryptionConfig,
    kubeconfig::KubeConfig,
    pki::Pki,
    process::{Process, Stoppable},
    LOCALHOST,
};
use failure::{bail, Fallible};
use incdoc::incdoc;
use log::{debug, info};
use std::{
    fs::{self, create_dir_all},
    path::Path,
    process::{Command, Stdio},
};

pub struct APIServer {
    process: Process,
}

impl APIServer {
    pub fn new(
        config: &Config,
        ip: &str,
        pki: &Pki,
        encryptionconfig: &EncryptionConfig,
        kubeconfig: &KubeConfig,
    ) -> Fallible<APIServer> {
        info!("Starting API Server");

        let dir = config.root.join("apiserver");
        create_dir_all(&dir)?;

        let mut process = Process::new(
            config,
            &[
                "kube-apiserver".to_owned(),
                format!("--advertise-address={}", ip),
                "--allow-privileged=true".to_owned(),
                "--audit-log-maxage=30".to_owned(),
                "--audit-log-maxbackup=3".to_owned(),
                "--audit-log-maxsize=100".to_owned(),
                format!("--audit-log-path={}", dir.join("audit.log").display()),
                "--authorization-mode=Node,RBAC".to_owned(),
                "--bind-address=0.0.0.0".to_owned(),
                format!("--client-ca-file={}", pki.ca.cert().display()),
                format!("--etcd-cafile={}", pki.ca.cert().display()),
                format!("--etcd-certfile={}", pki.apiserver.cert().display()),
                format!("--etcd-keyfile={}", pki.apiserver.key().display()),
                format!("--etcd-servers=https://{}:2379", LOCALHOST),
                "--event-ttl=1h".to_owned(),
                format!(
                    "--encryption-provider-config={}",
                    encryptionconfig.path.display()
                ),
                format!(
                    "--kubelet-certificate-authority={}",
                    pki.ca.cert().display()
                ),
                format!(
                    "--kubelet-client-certificate={}",
                    pki.apiserver.cert().display()
                ),
                format!(
                    "--kubelet-client-key={}",
                    pki.apiserver.key().display()
                ),
                "--kubelet-https=true".to_owned(),
                "--runtime-config=api/all".to_owned(),
                format!(
                    "--service-account-key-file={}",
                    pki.service_account.cert().display()
                ),
                format!(
                    "--service-cluster-ip-range={}",
                    config.kube.service_cidr
                ),
                format!("--tls-cert-file={}", pki.apiserver.cert().display()),
                format!(
                    "--tls-private-key-file={}",
                    pki.apiserver.key().display()
                ),
                "--v=2".to_owned(),
            ],
        )?;

        process.wait_ready("etcd ok")?;
        Self::setup_rbac(&dir, &kubeconfig.admin)?;
        info!("API Server is ready");
        Ok(APIServer { process })
    }

    fn setup_rbac(dir: &Path, admin_config: &Path) -> Fallible<()> {
        debug!("Creating API Server RBAC rule for kubelet");
        let yml = incdoc!(r#"---
apiVersion: rbac.authorization.k8s.io/v1beta1
kind: ClusterRole
metadata:
  annotations:
    rbac.authorization.kubernetes.io/autoupdate: \"true\"
  labels:
    kubernetes.io/bootstrapping: rbac-defaults
  name: system:kube-apiserver-to-kubelet
rules:
  - apiGroups:
      - ""
    resources:
      - nodes/proxy
      - nodes/stats
      - nodes/log
      - nodes/spec
      - nodes/metrics
    verbs:
      - "*"
---
apiVersion: rbac.authorization.k8s.io/v1beta1
kind: ClusterRoleBinding
metadata:
  name: system:kube-apiserver
  namespace: ""
roleRef:
  apiGroup: rbac.authorization.k8s.io
  kind: ClusterRole
  name: system:kube-apiserver-to-kubelet
subjects:
  - apiGroup: rbac.authorization.k8s.io
    kind: User
    name: kubernetes"#);
        let yml_file = dir.join("rbac.yml");
        fs::write(&yml_file, yml)?;

        let status = Command::new("kubectl")
            .arg("apply")
            .arg(format!("--kubeconfig={}", admin_config.display()))
            .arg("-f")
            .arg(yml_file)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()?;
        if !status.success() {
            bail!("kubectl apply command failed");
        }

        debug!("API Server RBAC rule created");
        Ok(())
    }
}

impl Stoppable for APIServer {
    fn stop(&mut self) {
        self.process.stop();
    }
}
