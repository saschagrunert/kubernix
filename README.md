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
log/
nix/
pki/
proxy/
scheduler/
```

For example, the log files for the different running components are now
available within the `log` directory:

```
> ls -1 log
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

| CLI argument         | Description                                                | Default         | Environment Variable    |
| -------------------- | ---------------------------------------------------------- | --------------- | ----------------------- |
| `-r, --root`         | Path where all the runtime data is stored                  | `kubernix-run`  | `KUBERNIX_ROOT`         |
| `-l, --log-level`    | Logging verbosity                                          | `info`          | `KUBERNIX_LOG_LEVEL`    |
| `-c, --crio-cidr`    | CIDR used for the CRI-O CNI network                        | `10.100.0.0/16` | `KUBERNIX_CRIO_CIDR`    |
| `-u, --cluster-cidr` | CIDR used for the whole cluster network                    | `10.200.0.0/16` | `KUBERNIX_CLUSTER_CIDR` |
| `-s, --service-cidr` | CIDR used for the service network                          | `10.50.0.0/24`  | `KUBERNIX_SERVICE_CIDR` |
| `-o, --overlay`      | Nix package overlay to be used                             |                 | `KUBERNIX_OVERLAY`      |
| `-p, --packages`     | Additional Nix dependencies to be added to the environment |                 | `KUBERNIX_PACKAGES`     |
| `-i, --impure`       | Do not clear the current env during bootstrap              | `false`         |                         |

Please ensure that the CIDRs are not overlapping with existing local networks
and that your setup has access to the internet.

#### Overlays

Overlays provide a method to extend and change Nix derivations. This means, that
we're able to change dependencies during the cluster bootstrapping process. For
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
[INFO  kubernix] Nix environment not found, bootstrapping one
[INFO  kubernix] Bootstrapping cluster inside nix environment
...
> helm init
> helm version
Client: &version.Version{SemVer:"v2.14.3", GitCommit:"", GitTreeState:"clean"}
Server: &version.Version{SemVer:"v2.14.3", GitCommit:"0e7f3b6637f7af8fcfddb3d2941fcc7cbebb0085", GitTreeState:"clean"}
```

All available packages are listed in the [official Nix website][21].

[20]: https://helm.sh
[21]: https://nixos.org/nixos/packages.html?channel=nixpkgs-unstable

#### Purity

If you still want to access some system packages inside the interactive shell,
then it is possible to run KuberNix in non-pure mode via the command line flag
`--impure, -i`. This is not recommended and can have negative impact on the
overall cluster bootstrapping process.

## Contributing

You want to contribute to this project? Wow, thanks! So please just fork it and
send me a pull request.
