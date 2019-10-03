<img src=".github/kubernix.png" width="300px"></img>

[![CircleCI](https://circleci.com/gh/saschagrunert/kubernix.svg?style=shield)](https://circleci.com/gh/saschagrunert/kubernix)
[![Docs master](https://img.shields.io/badge/doc-master-orange.svg)](https://saschagrunert.github.io/kubernix/doc/kubernix/index.html)
[![Docs release](https://docs.rs/kubernix/badge.svg)](https://docs.rs/kubernix)
[![Coverage](https://codecov.io/gh/saschagrunert/kubernix/branch/master/graph/badge.svg)](https://codecov.io/gh/saschagrunert/kubernix)
[![Dependencies](https://deps.rs/repo/github/saschagrunert/kubernix/status.svg)](https://deps.rs/repo/github/saschagrunert/kubernix)
[![Crates.io](https://img.shields.io/crates/v/kubernix.svg)](https://crates.io/crates/kubernix)
[![License MIT](https://img.shields.io/badge/license-MIT-blue.svg)](https://github.com/saschagrunert/kubernix/blob/master/LICENSE)

## Kubernetes development cluster bootstrapping with Nix packages

This project aims to provide you **single dependency**, single node
[Kubernetes][1] clusters for local testing, experimenting and development
purposes.

[1]: https://kubernetes.io

### About

Do you ever heard from [Nix][2], the functional package manager? Don't worry if
not, all you need to know is that it provides all the third party dependencies
for this project, pinned to a dedicated and reproducible version.

[2]: https://nixos.org/nix

KuberNix itself is the Rusty helper program, which takes care of bootstrapping
the Kubernetes cluster, passing the right configuration parameters around and
keeping track of the running processes.

### What is inside

The following technology stack is currently being used:

| Application       |  Purpose               | Version    |
| ----------------- | ---------------------- | ---------- |
| [Kubernetes][10]  | Cluster Orchestration  | v1.15.4    |
| [CRI-O][11]       | Container Runtime      | v1.15.2    |
| [runc][12]        | Container Runtime      | v1.0.0-rc8 |
| [cri-tools][13]   | CRI Manipulation Tool  | v1.15.0    |
| [CNI Plugins][14] | Container Networking   | v0.8.2     |
| [etcd][15]        | Database Backend       | v3.3.13    |
| [CoreDNS][16]     | Kubernetes DNS Support | v1.6.4     |

[10]: https://github.com/kubernetes/kubernetes
[11]: https://github.com/cri-o/cri-o
[12]: https://github.com/opencontainers/runc
[13]: https://github.com/kubernetes-sigs/cri-tools
[14]: https://github.com/containernetworking/plugins
[15]: https://github.com/etcd-io/etcd
[16]: https://github.com/coredns/coredns

Some other tools are not explicitly mentioned here, like [CFSSL][17] for the
certificate generation.

[17]: https://github.com/cloudflare/cfssl

### Single Dependency

As already mentioned, there is only one single dependency needed to run this
project: **Nix**. To setup Nix, simply run:

```shell
$ curl https://nixos.org/nix/install | sh
```

Please make sure to follow the instructions output by the script.

### Getting Started

#### Cluster Bootstrap

To bootstrap your first cluster, download one of the latest release binaries or
build the application via:

```shell
$ make build-release
```

The binary should now be available in the `target/release/kubernix` directory of
the project.

After the successful binary retrieval, start KuberNix by running it as `root`:

```
$ sudo kubernix
```

KuberNix will now take care that the Nix environment gets correctly setup,
downloads the needed binaries and starts the cluster. Per default it will create
a directory called `kubernix` in the current path which contains all necessary
data for the cluster.

#### Shell Environment

If everything went fine, you should be dropped into a new bash-shell session,
like this:

```
[INFO  kubernix] Everything is up and running
[INFO  kubernix] Spawning interactive shell
[INFO  kubernix] Please be aware that the cluster gets destroyed if you exit the shell
>
```

Now you can access your cluster via tools like `kubectl`:

```
> kubectl get pods --all-namespaces
NAMESPACE     NAME                       READY   STATUS    RESTARTS   AGE
kube-system   coredns-85d84dd694-xz997   1/1     Running   0          102s
```

The log files for the different running components are now available within the
current working directory, too:

```
> ls -1
crio.log
etcd.log
kube-apiserver.log
kube-controller-manager.log
kubelet.log
kube-proxy.log
kube-scheduler.log
```

If you want to spawn an additional shell session, simply run `kubernix shell` in
the same directory as the initial bootstrap.

```
$ sudo kubernix shell
[INFO  kubernix] Spawning new kubernix shell in 'kubernix-run'
> kubectl run --generator=run-pod/v1 --image=alpine -it alpine sh
If you don't see a command prompt, try pressing enter.
/ #
```

This means that you can spawn as many shells as you want to.

#### Cleanup

The whole cluster gets automatically destroyed if you exit the bash session from
the initial process:

```
> exit
[INFO  kubernix] Cleaning up
```

Please note that the directory where all the data is stored is not being
removed after the exit of KuberNix. This means that you're still able to
access the log and configuration files for further processing.

### Configuration

KuberNix has some configuration possibilities, which are currently:

| CLI argument         | Description                               | Default         |
| -------------------- | ----------------------------------------- | --------------- |
| `-r, --root`         | Path where all the runtime data is stored | `kubernix-run`  |
| `-l, --log-level`    | Logging verbosity                         | `info`          |
| `-c, --crio-cidr`    | CIDR used for the CRI-O CNI network       | `10.100.0.0/16` |
| `-u, --cluster-cidr` | CIDR used for the whole cluster network   | `10.200.0.0/16` |
| `-s, --service-cidr` | CIDR used for the service network         | `10.50.0.0/24`  |

Please ensure that the CIDRs are not overlapping with existing local networks
and that your setup has access to the internet.

## Contributing

You want to contribute to this project? Wow, thanks! So please just fork it and
send me a pull request.
