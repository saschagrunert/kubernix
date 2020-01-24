<img src=".github/kubernix.png" width="300px"></img>

[![CircleCI](https://circleci.com/gh/saschagrunert/kubernix.svg?style=shield)](https://circleci.com/gh/saschagrunert/kubernix)
[![Docs master](https://img.shields.io/badge/doc-master-orange.svg)](https://saschagrunert.github.io/kubernix/doc/kubernix/index.html)
[![Docs release](https://docs.rs/kubernix/badge.svg)](https://docs.rs/kubernix)
[![Coverage](https://codecov.io/gh/saschagrunert/kubernix/branch/master/graph/badge.svg)](https://codecov.io/gh/saschagrunert/kubernix)
[![Dependencies](https://deps.rs/repo/github/saschagrunert/kubernix/status.svg)](https://deps.rs/repo/github/saschagrunert/kubernix)
[![Crates.io](https://img.shields.io/crates/v/kubernix.svg)](https://crates.io/crates/kubernix)
[![License MIT](https://img.shields.io/badge/license-MIT-blue.svg)](https://github.com/saschagrunert/kubernix/blob/master/LICENSE)

## Kubernetes development cluster bootstrapping with Nix packages

This project aims to provide **single dependency** [Kubernetes][1] clusters
for local testing, experimenting and development purposes.

[1]: https://kubernetes.io

Moving pictures are worth more than thousand words, so here is a short demo:

![demo](.github/kubernix.svg)

### Nix?

Have you ever heard about [Nix][2], the functional package manager?

In case you haven't, don’t worry – the important thing is that it provides all the third-party
dependencies needed for this project, pinned to a dedicated version. This guarantees stable,
reproducible installations.

[2]: https://nixos.org/nix

KuberNix itself is a Rusty helper program, which takes care of bootstrapping
the Kubernetes cluster, passing the right configuration parameters around and
keeping track of the running processes.

### What is inside

The following technology stack is currently being used:

| Application     | Version      |
| --------------- | ------------ |
| cfssl           | v1.3.2       |
| cni-plugins     | v0.8.4       |
| conmon          | v2.0.9       |
| conntrack-tools | v1.4.5       |
| cri-o           | v1.16.1      |
| cri-tools       | v1.17.0      |
| etcd            | v3.3.13      |
| iproute2        | v5.4.0       |
| iptables        | v1.8.4       |
| kmod            | v26          |
| kubernetes      | v1.16.5      |
| nss-cacert      | v3.48        |
| podman          | v1.7.0       |
| runc            | v1.0.0-rc10  |
| socat           | v1.7.3.3     |
| sysctl          | v1003.1.2008 |
| util-linux      | v2.33.2      |

Some other tools are not explicitly mentioned here, because they are no
first-level dependencies.

### Single Dependency

#### With Nix

As already mentioned, there is only one single dependency needed to run this
project: **Nix**. To setup Nix, simply run:

```shell
$ curl https://nixos.org/nix/install | sh
```

Please make sure to follow the instructions output by the script.

#### With the Container Runtime of your Choice

It is also possible to run KuberNix in the container runtime of your choice. To
do this, simply grab the latest image from [`saschagrunert/kubernix`][40].
Please note that running KuberNix inside a container image requires to run
`privileged` mode and `host` networking. For example, we can run KuberNix with
[podman][41] like this:

[40]: https://cloud.docker.com/u/saschagrunert/repository/docker/saschagrunert/kubernix
[41]: https://github.com/containers/libpod

```
$ sudo podman run \
    --net=host \
    --privileged \
    -it docker.io/saschagrunert/kubernix:latest
```

### Getting Started

#### Cluster Bootstrap

To bootstrap your first cluster, download one of the latest [release binaries][18] or
build the application via:

[18]: https://github.com/saschagrunert/kubernix/releases/latest

```shell
$ make build-release
```

The binary should now be available in the `target/release/kubernix` directory of
the project. Alternatively, install the application via `cargo install kubernix`.

After the successful binary retrieval, start KuberNix by running it as `root`:

```
$ sudo kubernix
```

KuberNix will now take care that the Nix environment gets correctly setup,
downloads the needed binaries and starts the cluster. Per default it will create
a directory called `kubernix-run` in the current path which contains all necessary
data for the cluster.

#### Shell Environment

If everything went fine, you should be dropped into a new shell session,
like this:

```
[INFO ] Everything is up and running
[INFO ] Spawning interactive shell
[INFO ] Please be aware that the cluster stops if you exit the shell
>
```

Now you can access your cluster via tools like `kubectl`:

```
> kubectl get pods --all-namespaces
NAMESPACE     NAME                       READY   STATUS    RESTARTS   AGE
kube-system   coredns-85d84dd694-xz997   1/1     Running   0          102s
```

All configuration files have been written to the target directory, which is now
the current one:

```
> ls -1
apiserver/
controllermanager/
coredns/
crio/
encryptionconfig/
etcd/
kubeconfig/
kubelet/
kubernix.env
kubernix.toml
nix/
pki/
policy.json
proxy/
scheduler/
```

For example, the log files for the different running components are now
available within their corresponding directory:

```
> ls -1 **.log
apiserver/kube-apiserver.log
controllermanager/kube-controller-manager.log
crio/crio.log
etcd/etcd.log
kubelet/kubelet.log
proxy/kube-proxy.log
scheduler/kube-scheduler.log
```

If you want to spawn an additional shell session, simply run `kubernix shell` in
the same directory as where the initial bootstrap happened.

```
$ sudo kubernix shell
[INFO  kubernix] Spawning new kubernix shell in 'kubernix-run'
> kubectl run --generator=run-pod/v1 --image=alpine -it alpine sh
If you don't see a command prompt, try pressing enter.
/ #
```

This means that you can spawn as many shells as you want to.

#### Cleanup

The whole cluster gets automatically destroyed if you exit the shell session
from the initial process:

```
> exit
[INFO ] Cleaning up
```

Please note that the directory where all the data is stored is not being
removed after the exit of KuberNix. This means that you’re still able to
access the log and configuration files for further processing. If you start
the cluster again, then the cluster files will be reused. This is especially
handy if you want to test configuration changes.

#### Restart

If you start KuberNix again in the same run directory, then it will re-use the
configuration during the cluster bootstrapping process. This means that you
can modify all data inside the run root for testing and debugging purposes. The
startup of the individual components will be initiated by YAML files called
`run.yml`, which are available inside the directories of the corresponding
components. For example, etc gets started via:

```
> cat kubernix-run/etcd/run.yml
```

```yml
---
command: /nix/store/qlbsv0hvi0j5qj3631dzl9srl75finlk-etcd-3.3.13-bin/bin/etcd
args:
  - "--advertise-client-urls=https://127.0.0.1:2379"
  - "--client-cert-auth"
  - "--data-dir=/…/kubernix-run/etcd/run"
  - "--initial-advertise-peer-urls=https://127.0.0.1:2380"
  - "--initial-cluster-state=new"
  - "--initial-cluster-token=etcd-cluster"
  - "--initial-cluster=etcd=https://127.0.0.1:2380"
  - "--listen-client-urls=https://127.0.0.1:2379"
  - "--listen-peer-urls=https://127.0.0.1:2380"
  - "--name=etcd"
  - "--peer-client-cert-auth"
  - "--cert-file=/…/kubernix-run/pki/kubernetes.pem"
  - "--key-file=/…/kubernix-run/pki/kubernetes-key.pem"
  - "--peer-cert-file=/…/kubernix-run/pki/kubernetes.pem"
  - "--peer-key-file=/…/kubernix-run/pki/kubernetes-key.pem"
  - "--peer-trusted-ca-file=/…/kubernix-run/pki/ca.pem"
  - "--trusted-ca-file=/…/kubernix-run/pki/ca.pem"
```

### Configuration

KuberNix has some configuration possibilities, which are currently:

| CLI argument      | Description                                                                         | Default        | Environment Variable         |
| ----------------- | ----------------------------------------------------------------------------------- | -------------- | ---------------------------- |
| `-r, --root`      | Path where all the runtime data is stored                                           | `kubernix-run` | `KUBERNIX_ROOT`              |
| `-l, --log-level` | Logging verbosity                                                                   | `info`         | `KUBERNIX_LOG_LEVEL`         |
| `-c, --cidr`      | CIDR used for the cluster network                                                   | `10.10.0.0/16` | `KUBERNIX_CIDR`              |
| `-s, --shell`     | The shell executable to be used                                                     | `$SHELL`/`sh`  | `KUBERNIX_SHELL`             |
| `-e, --no-shell`  | Do not spawn an interactive shell after bootstrap                                   | `false`        | `KUBERNIX_NO_SHELL`          |
| `-n, --nodes`     | The number of nodes to be registered                                                | `1`            | `KUBERNIX_NODES`             |
| `-u, --runtime`   | The container runtime to be used for the nodes, irrelevant if `nodes` equals to `1` | `podman`       | `KUBERNIX_CONTAINER_RUNTIME` |
| `-o, --overlay`   | Nix package overlay to be used                                                      |                | `KUBERNIX_OVERLAY`           |
| `-p, --packages`  | Additional Nix dependencies to be added to the environment                          |                | `KUBERNIX_PACKAGES`          |

Please ensure that the CIDR is not overlapping with existing local networks and
that your setup has access to the internet. The CIDR will be automatically split
up over the necessary cluster components.

#### Multinode Support

It is possible to spawn multiple worker nodes, too. To do this, simply adjust
the `-n, --nodes` command line argument as well as your preferred container
runtime via `-u, --runtime`. The default runtime is [podman][41], but every
other Docker drop-in replacement should work out of the box.

#### Overlays

Overlays provide a method to extend and change Nix derivations. This means, that
we’re able to change dependencies during the cluster bootstrapping process. For
example, we can exchange the used CRI-O version to use a local checkout by
writing this simple `overlay.nix`:

```nix
self: super: {
  cri-o = super.cri-o.overrideAttrs(old: {
    src = ../path/to/go/src/github.com/cri-o/cri-o;
  });
}
```

Now we can run KuberNix with the `--overlay, -o` command line argument:

```
$ sudo kubernix --overlay overlay.nix
[INFO  kubernix] Nix environment not found, bootstrapping one
[INFO  kubernix] Using custom overlay 'overlay.nix'
these derivations will be built:
  /nix/store/9jb43i2mqjc94mbx30d9nrx529w6lngw-cri-o-1.15.2.drv
  building '/nix/store/9jb43i2mqjc94mbx30d9nrx529w6lngw-cri-o-1.15.2.drv'...
```

Using this technique makes it easy for daily development of Kubernetes
components, by simply changing it to local paths or trying out new versions.

#### Additional Packages

It is also possible to add additional packages to the KuberNix environment by
specifying them via the `--packages, -p` command line parameter. This way you
can easily utilize additional tools in a reproducible way. For example, when to
comes to using always the same [Helm][20] version, you could simply run:

```
$ sudo kubernix -p kubernetes-helm
[INFO ] Nix environment not found, bootstrapping one
[INFO ] Bootstrapping cluster inside nix environment
…
> helm init
> helm version
Client: &version.Version{SemVer:"v2.14.3", GitCommit:"", GitTreeState:"clean"}
Server: &version.Version{SemVer:"v2.14.3", GitCommit:"0e7f3b6637f7af8fcfddb3d2941fcc7cbebb0085", GitTreeState:"clean"}
```

All available packages are listed on the [official Nix index][21].

[20]: https://helm.sh
[21]: https://nixos.org/nixos/packages.html?channel=nixpkgs-unstable

## Contributing

You want to contribute to this project? Wow, thanks! So please just fork it and
send me a pull request.
