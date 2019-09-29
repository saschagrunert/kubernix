use crate::{config::Config, kubeconfig::KubeConfig};
use failure::{bail, Fallible};
use incdoc::incdoc;
use log::info;
use std::{
    fs::{self, create_dir_all},
    process::{Command, Stdio},
};

pub struct CoreDNS;

impl CoreDNS {
    pub fn apply(config: &Config, kubeconfig: &KubeConfig) -> Fallible<()> {
        info!("Deploying CoreDNS");

        let dir = config.root.join("coredns");
        create_dir_all(&dir)?;

        let yml = incdoc!(format!(
            r#"
               ---
               apiVersion: v1
               kind: ServiceAccount
               metadata:
                 name: coredns
                 namespace: kube-system
               ---
               apiVersion: rbac.authorization.k8s.io/v1
               kind: ClusterRole
               metadata:
                 labels:
                   kubernetes.io/bootstrapping: rbac-defaults
                 name: system:coredns
               rules:
               - apiGroups:
                 - ""
                 resources:
                 - endpoints
                 - services
                 - pods
                 - namespaces
                 verbs:
                 - list
                 - watch
               - apiGroups:
                 - ""
                 resources:
                 - nodes
                 verbs:
                 - get
               ---
               apiVersion: rbac.authorization.k8s.io/v1
               kind: ClusterRoleBinding
               metadata:
                 annotations:
                   rbac.authorization.kubernetes.io/autoupdate: "true"
                 labels:
                   kubernetes.io/bootstrapping: rbac-defaults
                 name: system:coredns
               roleRef:
                 apiGroup: rbac.authorization.k8s.io
                 kind: ClusterRole
                 name: system:coredns
               subjects:
               - kind: ServiceAccount
                 name: coredns
                 namespace: kube-system
               ---
               apiVersion: v1
               kind: ConfigMap
               metadata:
                 name: coredns
                 namespace: kube-system
               data:
                 Corefile: |
                   .:53 {{
                       errors
                       health
                       ready
                       kubernetes cluster.local in-addr.arpa ip6.arpa {{
                         pods insecure
                         fallthrough in-addr.arpa ip6.arpa
                       }}
                       prometheus :9153
                       cache 30
                       loop
                       reload
                       loadbalance
                   }}
               ---
               apiVersion: apps/v1
               kind: Deployment
               metadata:
                 name: coredns
                 namespace: kube-system
                 labels:
                   k8s-app: core-dns
                   kubernetes.io/name: "CoreDNS"
               spec:
                 replicas: 2
                 strategy:
                   type: RollingUpdate
                   rollingUpdate:
                     maxUnavailable: 1
                 selector:
                   matchLabels:
                     k8s-app: core-dns
                 template:
                   metadata:
                     labels:
                       k8s-app: core-dns
                   spec:
                     priorityClassName: system-cluster-critical
                     serviceAccountName: coredns
                     tolerations:
                       - key: "CriticalAddonsOnly"
                         operator: "Exists"
                     nodeSelector:
                       beta.kubernetes.io/os: linux
                     containers:
                     - name: coredns
                       image: coredns/coredns:1.6.3
                       imagePullPolicy: IfNotPresent
                       resources:
                         limits:
                           memory: 170Mi
                         requests:
                           cpu: 100m
                           memory: 70Mi
                       args: [ "-conf", "/etc/coredns/Corefile" ]
                       volumeMounts:
                       - name: config-volume
                         mountPath: /etc/coredns
                         readOnly: true
                       ports:
                       - containerPort: 53
                         name: dns
                         protocol: UDP
                       - containerPort: 53
                         name: dns-tcp
                         protocol: TCP
                       - containerPort: 9153
                         name: metrics
                         protocol: TCP
                       securityContext:
                         allowPrivilegeEscalation: false
                         capabilities:
                           add:
                           - NET_BIND_SERVICE
                           drop:
                           - all
                         readOnlyRootFilesystem: true
                       livenessProbe:
                         httpGet:
                           path: /health
                           port: 8080
                           scheme: HTTP
                         initialDelaySeconds: 60
                         timeoutSeconds: 5
                         successThreshold: 1
                         failureThreshold: 5
                       readinessProbe:
                         httpGet:
                           path: /ready
                           port: 8181
                           scheme: HTTP
                     dnsPolicy: Default
                     volumes:
                       - name: config-volume
                         configMap:
                           name: coredns
                           items:
                           - key: Corefile
                             path: Corefile
               ---
               apiVersion: v1
               kind: Service
               metadata:
                 name: core-dns
                 namespace: kube-system
                 annotations:
                   prometheus.io/port: "9153"
                   prometheus.io/scrape: "true"
                 labels:
                   k8s-app: core-dns
                   kubernetes.io/cluster-service: "true"
                   kubernetes.io/name: "CoreDNS"
               spec:
                 selector:
                   k8s-app: core-dns
                 clusterIP: {}
                 ports:
                 - name: dns
                   port: 53
                   protocol: UDP
                 - name: dns-tcp
                   port: 53
                   protocol: TCP
                 - name: metrics
                   port: 9153
                   protocol: TCP
                "#,
            config.kube.cluster_dns
        ));
        let yml_file = dir.join("coredns.yml");
        fs::write(&yml_file, yml)?;

        let status = Command::new("kubectl")
            .arg("apply")
            .arg(format!("--kubeconfig={}", kubeconfig.admin.display()))
            .arg("-f")
            .arg(yml_file)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()?;
        if !status.success() {
            bail!("kubectl apply command failed");
        }

        info!("CoreDNS deployed");
        Ok(())
    }
}
