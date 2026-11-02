# Configuration

KuberNix can be configured via command line arguments or environment variables:

| CLI argument          | Description                                                                 | Default        | Environment Variable         |
| --------------------- | --------------------------------------------------------------------------- | -------------- | ---------------------------- |
| `-r, --root`          | Path where all the runtime data is stored                                   | `kubernix-run` | `KUBERNIX_ROOT`              |
| `-l, --log-level`     | Logging verbosity (`off`, `error`, `warn`, `info`, `debug` or `trace`)      | `info`         | `KUBERNIX_LOG_LEVEL`         |
| `--log-format`        | Log output format (`text` or `json`)                                        | `text`         | `KUBERNIX_LOG_FORMAT`        |
| `-c, --cidr`          | CIDR used for the cluster network, host bits are ignored                    | `10.10.0.0/16` | `KUBERNIX_CIDR`              |
| `-s, --shell`         | The shell executable to be used                                             | `$SHELL`/`sh`  | `KUBERNIX_SHELL`             |
| `--no-shell`          | Do not spawn an interactive shell after bootstrap                           | `false`        | `KUBERNIX_NO_SHELL`          |
| `-n, --nodes`         | The number of nodes to be registered (`1` to `64`)                          | `1`            | `KUBERNIX_NODES`             |
| `--container-runtime` | Container runtime for the node containers, only used in multinode root mode | `podman`       | `KUBERNIX_CONTAINER_RUNTIME` |
| `-o, --overlay`       | Nix package overlay to be used                                              |                | `KUBERNIX_OVERLAY`           |
| `-d, --dockerfile`    | Custom Dockerfile for the node containers, only used in multinode root mode |                | `KUBERNIX_DOCKERFILE`        |
| `-p, --packages`      | Additional Nix packages to be added to the environment, comma separated     |                | `KUBERNIX_PACKAGES`          |
| `--cri-runtime`       | CRI runtime to use (`crio` or `containerd`)                                 | `crio`         | `KUBERNIX_CRI_RUNTIME`       |
| `--oci-runtime`       | Default OCI runtime (`crun` or `runc`)                                      | `crun`         | `KUBERNIX_OCI_RUNTIME`       |
| `-a, --addons`        | Cluster addons to deploy, comma separated (`coredns`), `none` disables all  | `coredns`      | `KUBERNIX_ADDONS`            |

Options which take a list accept comma separated values and can be repeated, for
example `--packages=kubernetes-helm,jq` or `-p kubernetes-helm -p jq`, but not
space separated values. The same applies to their environment variables, like
`KUBERNIX_PACKAGES=kubernetes-helm,jq`. An empty value, like `--packages=` or
`KUBERNIX_OVERLAY=`, sets no packages, overlay or Dockerfile. Boolean
environment variables like `KUBERNIX_NO_SHELL` take `true` or `false`, where
`false` is the same as not setting the variable. Options on the command line
take precedence over environment variables, which also applies to `--shell` and
`--no-shell`: setting both on the command line or both via the environment is an
error. The `--root` option can also be given after any subcommand.

The subcommands have these options:

| Subcommand        | Option          | Description                                                   | Default |
| ----------------- | --------------- | ------------------------------------------------------------- | ------- |
| `kubernix shell`  | `-- COMMAND…`   | Run the command in the cluster environment instead of a shell |         |
| `kubernix stop`   | `-t, --timeout` | Seconds to wait for the cleanup to finish                     | `180`   |
| `kubernix status` | `--output`      | Output format (`text` or `json`)                              | `text`  |

Environment variables starting with `_KUBERNIX_` pass state between the internal
stages of KuberNix. They are not part of the interface and must not be set.

Please ensure that the CIDR is not overlapping with existing local networks and
that your setup has access to the internet. The CIDR will be automatically split
up over the necessary cluster components: the first half is the cluster network,
which gets split equally into the pod networks of the nodes, and the second half
contains the service network, which is at most a `/16`. For example, the default
`10.10.0.0/16` results in the pod network `10.10.0.0/17` and the service network
`10.10.128.0/17` for a single node, while three nodes get the pod networks
`10.10.0.0/19`, `10.10.32.0/19` and `10.10.64.0/19`.

## OCI Runtimes

Both [crun][1] and [runc][2] are registered as runtime handlers named `crun` and
`runc` for CRI-O as well as containerd, each with its own state directory
(`<cri>/<node>/crun` and `<cri>/<node>/runc`). The `--oci-runtime` option
selects the handler used for pods without a runtime class, while pods can select
the other one via a [RuntimeClass][3]:

```yaml
apiVersion: node.k8s.io/v1
kind: RuntimeClass
metadata:
  name: runc
handler: runc
```

If the other runtime cannot be found, then KuberNix warns and does not register
its handler, while a missing default runtime is an error. This works in single
node, multinode and rootless mode. The container runtime used for the node
containers in multinode mode (`--container-runtime`) is not affected by this
option.

CRI-O monitors the containers of a pod with one [conmon-rs][] instance (the
`pod` runtime type), which it creates for the infra container. CRI-O 1.37 runs a
pod without infra container with the default handler instead of the one of its
RuntimeClass, so KuberNix keeps the infra containers (`drop_infra_ctr = false`)
until CRI-O passes the handler on. This costs one pause container per pod, and
every node pulls the pause image.

Workloads behave the same with both CRI runtimes: containers get the default
capabilities of containerd, may bind privileged ports and, outside of rootless
mode, open ICMP echo sockets (`ping`). Short image names like `nginx` resolve to
`docker.io`. In rootless mode, the runtimes raise OOM scores below their own to
it, since they cannot lower them, so the kubelet keeps the order between
BestEffort, Burstable and Guaranteed pods only above that score. Neither CRI
runtime reads the configuration files of the host (for example `/etc/crio`,
`/etc/containerd` or the storage, registries and policy files in
`/etc/containers`), its OCI hooks or its CDI specs (`/etc/cdi` and
`/var/run/cdi`), and NRI is disabled. On purpose, the registry credentials of
the host (CRI-O only) and its registry certificate directories still apply, so
that private registries work like on the host: CRI-O reads
`/etc/containers/certs.d` and `/etc/docker/certs.d`, containerd reads the
`hosts.toml` files and certificates in `/etc/containerd/certs.d` only.

[1]: https://github.com/containers/crun
[2]: https://github.com/opencontainers/runc
[3]: https://kubernetes.io/docs/concepts/containers/runtime-class
[conmon-rs]: https://github.com/containers/conmon-rs

## Multinode Support

It is possible to spawn multiple worker nodes, too. To do this, simply adjust
the `-n, --nodes` command line argument. Every node gets its own CRI runtime and
kubelet. In rootless mode, they run as processes in the same user and network
namespace. In root mode, they run inside containers, which mount the Nix store
of the host read-only and use the `PATH` of the Nix environment, so the nodes
run the same binaries as the host without building or pulling an image which
contains them. The image of the containers only consists of a minimal root file
system (the `/etc/passwd` and `/etc/group` of root as well as `/run`, `/tmp` and
`/var/tmp`), which KuberNix imports on the first multinode run. The container
runtime can be selected via `--container-runtime`. The default runtime is
[podman][4], but other Docker compatible runtimes should work as well. The node
image is never pulled (`--pull=never`), it only exists locally.

The node containers are called `kubernix-<id>-<node>`, where `<id>` identifies
the run directory, and carry the labels `kubernix.root=<id>` and
`kubernix.root.path=<run directory>`, so clusters in different run directories
do not interfere (see [Cleanup](usage.md#cleanup) for leftover containers). The
node containers of 0.4, which are called `kubernix-node-<n>`, get removed on
start. Running ones may belong to a cluster of 0.4 and are not removed
automatically: if they block the ports, the start fails with the command which
removes them.

The nodes trust the same CA bundle for pulling images as the components on the
host: the one of `SSL_CERT_FILE`, otherwise the first system bundle like
`/etc/ssl/certs/ca-certificates.crt` or `/etc/pki/tls/certs/ca-bundle.crt`,
otherwise the one of the Nix environment. A bundle outside of the Nix store gets
mounted read-only to `/etc/ssl/certs/ca-certificates.crt`, so registry CAs
installed on the host work in multinode mode as well. The same applies to the
registry configuration which the CRI runtimes use on the host: the credentials
of root (`/run/containers/0/auth.json`, `/root/.config/containers/auth.json`,
`/root/.docker/config.json` and the file of `REGISTRY_AUTH_FILE`) and the
certificate directories `/etc/containers/certs.d`, `/etc/containerd/certs.d` and
`/etc/docker/certs.d` get mounted read-only if they exist. The proxy variables
(`HTTP_PROXY`, `HTTPS_PROXY`, `NO_PROXY` and their lower case variants) get
forwarded to the nodes as well.

A custom image can be built via `-d, --dockerfile`, for example to provide
additional files in the node containers. Its build context is the directory of
the Dockerfile, so it is best kept in a directory of its own. Run directories in
the build context, the one of the cluster and every other directory which
contains a `kubernix.toml`, contain the keys of their clusters. Podman excludes
them via an ignore file (`containerignore` in the run directory, which extends
the `.containerignore` or `.dockerignore` of the build context). Podman is
detected by the name of the executable after resolving symbolic links, or by its
`--version` output, so for example a `docker` command of podman-docker counts as
podman. Other container runtimes refuse a build context which contains run
directories: move the Dockerfile into its own directory or use podman. A build
context within a run directory is always refused. The Nix store and the
environment get mounted into the containers the same way, so the image does not
need to contain Nix. Note that the mounted store hides a `/nix/store` of the
image. The node images are tagged with a hash of their inputs
(`kubernix:base-<hash>`): the content of the default image, or the custom
Dockerfile together with the metadata (path, mode, size and modification time)
of all files in its build context, except the run directories. This includes
files which an ignore file excludes from the build, like a `.git` directory,
whose index changes with every `git status`. So the images get recreated
whenever the build context changes, which is why it should be small, and the
images of previous builds get removed afterwards. The tag gets determined once
per start, so changes of the build context while the cluster runs do not affect
the nodes.

All nodes share the network namespace of the host (or the one of rootlesskit),
where every node has its own bridge. The bridges masquerade the traffic leaving
the cluster network, while an iptables rule exempts the traffic between pods of
different nodes, so it is routed without NAT.

One kube-proxy serves all nodes and considers only the pods of the first node as
local. So Services with `internalTrafficPolicy: Local` or
`externalTrafficPolicy: Local` only reach the pods of the first node, and
traffic to them gets dropped if none of their pods runs there.

[4]: https://github.com/containers/podman

## Networking

The pod network of every node is a CNI bridge named `kx<id>.<number>`, like
`kx0123abcd.0` for the first node, where `<id>` identifies the run directory. So
clusters of different run directories never share bridges.

In root mode, KuberNix creates the bridges with their gateway address, the first
address of the pod CIDR of the node (like `10.10.0.1`), which the kubelets
report as node IP (`InternalIP`), and with a fixed MAC address derived from
`<id>` and the node number. The node IP does not change with the network of the
host, for example on a new DHCP lease, and it is not advertised to other
machines. The API server reaches the kubelets via the node IP and advertises the
node IP of the first node as endpoint of the `kubernetes` Service, so it also
starts without a default route. In rootless mode, the node IP is the address of
the network namespace of rootlesskit (`10.0.2.100`).

Before it creates the bridges, KuberNix removes the bridges which a former run
of the same run directory left behind. In root mode, it also removes the bridges
of other run directories and of former versions (`kubernix.<number>`) whose
address overlaps the CIDR, since the host would route the pod traffic via them.
Only a cluster which did not clean up, for example after a force quit, leaves
them behind, since only one cluster runs in root mode at a time (see below).
Bridges of other run directories which do not overlap are kept until the next
start in their run directory.

CoreDNS runs in the host network and listens only on the node IP: on port
`10053` for DNS, which its Service maps to port `53`, so it does not collide
with other DNS servers on the host, on `10054` and `10055` for the health and
readiness checks and on `9153` for the metrics. The node IP is not advertised,
but the host accepts packets for all of its addresses on every interface, so a
machine in the same local network which routes the CIDR via the host can reach
these ports, unless a firewall on the host blocks them. Pods with the `Default`
DNS policy, like CoreDNS for its upstream servers, use the resolver
configuration of the host. If it points to the stub resolver of systemd-resolved
on a loopback address, which pods cannot reach, the kubelets use
`/run/systemd/resolve/resolv.conf` instead.

The `hostPort` of pods works with both CRI runtimes: CRI-O maps the ports
itself, while containerd uses the CNI `portmap` plugin.

The components listen on fixed ports, which KuberNix checks before it changes
the system: `2379` and `2380` (etcd), `6443` (API server), `10257` (controller
manager), `10259` (scheduler), `10249` and `10256` (kube-proxy) and `11250` and
`12250` for the first kubelet, counting up for every further node. So only one
cluster runs in root mode at a time. If a port is in use by the cluster of
another run directory, `kubernix status --root <dir>` shows and
`kubernix stop --root <dir>` stops it.

In root mode, KuberNix locks `/run/kubernix-host-state.lock` before it changes
the system and keeps it locked until it has restored the system after the
cluster stopped. Meanwhile, another cluster in root mode fails to start with an
error, also while the components of the first one do not listen yet, instead of
removing its bridges. KuberNix saves the sysctls which it, the kubelets and
kube-proxy change in `/run/kubernix-host-state`, like
`net.ipv4.conf.all.route_localnet` and `kernel.panic`. When the cluster stops,
it restores them and removes the rules of kube-proxy via `kube-proxy --cleanup`.
After a force quit, the next run keeps the saved values and restores them when
it stops, like the values which could not be restored. KuberNix enables
`net.ipv4.ip_forward`, `net.bridge.bridge-nf-call-iptables` and
`net.bridge.bridge-nf-call-ip6tables` (loading `br_netfilter`) but does not
disable them again, like kubeadm and kind, since other services started
meanwhile, like Docker, libvirt or a VPN, may need them.

## Overlays

Overlays provide a method to extend and change Nix derivations. This means that
we’re able to change dependencies during the cluster bootstrapping process. For
example, we can build CRI-O from a fork by writing this `overlay.nix`:

```nix
self: super: {
  cri-o-unwrapped = super.cri-o-unwrapped.overrideAttrs (old: {
    src = super.fetchFromGitHub {
      owner = "my-user";
      repo = "cri-o";
      rev = "0123456789abcdef0123456789abcdef01234567";
      hash = "sha256-…";
    };
  });
}
```

The `cri-o` package only wraps `cri-o-unwrapped`, which contains the actual
build. The overlay replaces the default one in the generated Nix environment,
which is a flake evaluated in pure mode. This means that it cannot refer to
local paths outside of it, so sources have to be fetched with a fixed hash.

Now we can run KuberNix with the `--overlay, -o` command line argument:

```console
$ kubernix --overlay overlay.nix
[INFO ] Using custom overlay 'overlay.nix'
these 2 derivations will be built:
  /nix/store/…-cri-o-….drv
  /nix/store/…-cri-o-wrapper-….drv
building '/nix/store/…-cri-o-….drv'...
```

This makes it easy to try out patched or new versions of the cluster components.

## Additional Packages

It is also possible to add additional packages to the KuberNix environment by
specifying their nixpkgs attribute names via the `--packages, -p` command line
parameter. This way you can use additional tools in a reproducible way. For
example, to always use the same [Helm][5] version, run:

```console
$ kubernix -p kubernetes-helm
[INFO ] Bootstrapping cluster
…
> helm version
version.BuildInfo{Version:"v…", …}
```

Multiple packages can be passed comma separated or by repeating the option (see
[Configuration](#configuration)), for example `-p kubernetes-helm,jq`. A package
is a nixpkgs attribute path, like `jq` or `python3Packages.pyyaml`. All
available packages are listed on the [official Nix package search][6].

[5]: https://helm.sh
[6]: https://search.nixos.org/packages?channel=unstable
