# Usage

## Cluster Bootstrap

To bootstrap your first cluster, download the static binary for your
architecture (`kubernix-x86_64` or `kubernix-aarch64`) from the [latest
release][1] or build the application via:

[1]: https://github.com/saschagrunert/kubernix/releases/latest

```shell
$ make build-release
```

The binary is now available at `target/release/kubernix`. Alternatively, install
the application via `cargo install kubernix`. Downloaded release binaries can be
checked as described in [Verifying Releases](releases.md).

Then start KuberNix as a regular user (rootless mode) or as root (see
[Requirements](requirements.md)):

```console
$ kubernix
$ sudo env "PATH=$PATH" kubernix
```

KuberNix now sets up the Nix environment, downloads the needed binaries and
starts the cluster. By default, it creates a directory called `kubernix-run` in
the current path, which contains all data of the cluster.

When running as a non-root user, KuberNix uses `rootlesskit` to provide a user
namespace where all components run as fake root (see the
[requirements](requirements.md)). The cluster runs in its own network namespace,
which `slirp4netns` connects to the outside world. Only the API server port
(6443) is published to `127.0.0.1` on the host, so NodePort Services and host
ports are not reachable from the host, and the cluster cannot reach services
listening on the loopback interface of the host. The `--cidr` must not overlap
with the slirp4netns network `10.0.2.0/24`. Kernel modules cannot be loaded
explicitly from the user namespace. The kernel loads the netfilter modules for
the pod network and kube-proxy on demand, otherwise they have to be loaded on
the host (for example `iptable_nat` and `nf_conntrack`).

In multi-node rootless mode, all nodes run as direct processes (no container
isolation) with `--hostname-override` for node identity. Every kubelet runs in
its own mount namespace, because it always uses
`/var/lib/kubelet/device-plugins`, and writes its pod logs into its node
directory. Running with `sudo` gives container-based multi-node isolation.

## Shell Environment

If everything went fine, you should be dropped into a new shell session, like
this:

```text
[INFO ] Deploying CoreDNS in the background
[INFO ] Everything is up and running
[INFO ] Spawning interactive shell
[INFO ] Please be aware that the cluster stops if you exit the shell
>
```

CoreDNS gets deployed while the shell is already usable, so its pod may still be
starting. Now you can access your cluster via tools like `kubectl`:

```text
> kubectl get pods --all-namespaces
NAMESPACE     NAME                       READY   STATUS    RESTARTS   AGE
kube-system   coredns-85d84dd694-xz997   1/1     Running   0          102s
```

All configuration files have been written to the run directory, which is now the
current one:

```text
> ls -1p
apiserver/
controllermanager/
coredns/
crio/          # or containerd/ when using --cri-runtime=containerd
encryptionconfig/
etcd/
kubeconfig/
kubelet/
kubernix.env
kubernix.lock
kubernix.pgroups
kubernix.pid
kubernix.toml
nix/
nix-profile/
nix-profile.lock
pki/
policy.json
proxy/
scheduler/
```

For example, the log files for the different running components are now
available within their corresponding directory:

```text
> ls -1 */*.log */*/*.log
apiserver/audit.log
apiserver/kube-apiserver.log
controllermanager/kube-controller-manager.log
crio/<node>/crio.log  # or containerd/<node>/containerd.log
etcd/etcd.log
kubelet/<node>/kubelet.log
proxy/kube-proxy.log
scheduler/kube-scheduler.log
```

The CRI runtime and kubelet data is stored per node, where `<node>` is the
hostname for a single node cluster and `node-0`, `node-1`, … in multinode mode.
If a component runs inside a node container (multinode root mode), then its log
file is named after the container runtime, for example `crio/node-1/podman.log`.

The kubeconfigs embed the client keys, so they are only readable by their owner.
When running via `sudo`, the `kubeconfig/admin.kubeconfig` belongs to the
invoking user. The `pki` directory is only accessible by its owner.

If you want to spawn an additional shell session, simply run `kubernix shell` in
the same directory as where the initial bootstrap happened. In rootless mode,
the shell enters the namespaces of the running cluster via `nsenter` and keeps
the current working directory. If no cluster is running there, `kubernix shell`
exits with `3`, and it fails if the cluster is not ready yet. Only one KuberNix
instance can run per run directory, a second one fails to start and points to
`kubernix shell` instead. The shells of the cluster export `KUBERNIX_ROOT` with
the absolute path of the run directory, so `kubernix status`, `kubernix stop`
and `kubernix shell` work there from any directory. A new cluster cannot be
started from the shell of a rootless cluster, since KuberNix runs as root in its
user namespace: start it from a regular terminal instead.

```console
$ kubernix shell
[INFO ] Spawning new kubernix shell in: '/…/kubernix-run'
> kubectl run alpine --image=alpine -it --rm -- sh
If you don't see a command prompt, try pressing enter.
/ #
```

This means that you can spawn as many shells as you want to.

To run a single command in the same environment instead of an interactive shell,
for example in scripts or CI, pass it after `--`. KuberNix exits with the exit
code of the command:

```console
$ kubernix shell -- kubectl get nodes
NAME      STATUS   ROLES    AGE   VERSION
my-host   Ready    <none>   2m    v1.37.1
```

## Status

`kubernix status` shows whether a cluster is running in the run directory, as
well as its mode (`root` or `rootless`), the number of nodes, the CRI and OCI
runtimes, the API server endpoint and the readiness of the nodes:

```console
$ kubernix status
Root:        /home/user/kubernix-run
State:       running (PID 12345)
Mode:        rootless
Nodes:       1
CRI runtime: crio
OCI runtime: crun
API server:  https://127.0.0.1:6443
Node:        my-host Ready
```

The state is `starting` while KuberNix prepares the Nix environment and
bootstraps the cluster, and `stopped` if no KuberNix instance uses the run
directory. Use `kubernix status --output json` for a machine-readable output,
see [Status Output](stability.md#status-output).

## Cleanup

The whole cluster gets automatically destroyed if you exit the shell session
from the initial process:

```text
> exit
[INFO ] Cleaning up
…
[INFO ] Cleanup done
```

The same happens on `SIGTERM`, `SIGQUIT` or `SIGHUP`, as well as on `SIGINT`
(Ctrl+C) during the bootstrap or with `--no-shell`. KuberNix writes its PID to
`kubernix.pid` in the run directory once the cluster is ready and removes it
after the cleanup. If one of the components dies, KuberNix shuts the cluster
down with `--no-shell` and warns about it while the shell is open.

`kubernix stop` stops the cluster of the run directory from another terminal or
script. It sends `SIGTERM` to the KuberNix instance and waits until the cleanup
is done and the instance exited, up to `--timeout` seconds (default `180`). A
cluster which is still starting aborts its bootstrap instead, also while the Nix
environment gets evaluated, which removes the configuration of a new run
directory again. `kubernix stop` works from the shell of the cluster as well.

Another termination signal during the bootstrap or the cleanup, for example
pressing Ctrl+C again or running `kubernix stop` again, force quits KuberNix.
Signals within one second after the first one count as duplicates of it, and
`SIGHUP` never force quits, since a terminal or an SSH session may deliver a
signal more than once. If the cleanup did not start because of a signal, for
example after exiting the shell, it takes two signals. A force quit kills all
component processes immediately, restores `/etc/hosts` and removes the PID file,
but skips the remaining cleanup steps, so pods, node containers, mounts, network
interfaces or firewall rules may be left behind. The next start in the same run
directory removes these leftovers, like the component processes, node
containers, bridges, pod IP allocations and firewall rules, as well as the ones
of a killed KuberNix. `kubernix stop` removes the leftover node containers and
mounts of a root cluster as well once no cluster runs there anymore.

In rootless mode, the CRI storage (images and containers) contains files owned
by the subordinate IDs of the user, so it gets removed during the cleanup from
inside the user namespace, or by the next start after an unclean exit.

A cluster started as root can only be accessed as root. `kubernix status` works
for other users as well (the lock file `kubernix.lock` is only readable by root,
so they check the lock via `/proc/locks`), but `kubernix shell` and
`kubernix stop` (if the cluster runs) fail for them and print the command to run
them as root. It keeps the environment for the `KUBERNIX_*` variables, for
example:

```console
$ sudo -E env "PATH=$PATH" kubernix stop
```

`kubernix stop`, `kubernix status` and `kubernix shell` exit with `3` if no
cluster is running in the run directory, see
[Exit Codes](stability.md#exit-codes).

Please note that the run directory is not removed after the exit of KuberNix.
This means that you’re still able to access the log and configuration files for
further processing, and that the next start reuses the cluster (see
[Restart](#restart)).

To remove the run directory of a stopped root cluster, run:

```console
$ sudo rm -rf kubernix-run
```

The run directory of a rootless cluster may contain files owned by the
subordinate IDs of the user, for which a plain `rm -rf` fails with
`Permission denied`. Remove it from a user namespace which maps them instead:

```console
$ unshare --map-auto --map-root-user rm -rf kubernix-run
```

This requires `util-linux` 2.39 or newer (for example via
`nix shell nixpkgs#util-linux`). With `podman`,
`podman unshare rm -rf kubernix-run` works as well.

## Restart

If you start KuberNix again in the same run directory, then it reuses the
existing cluster: the etcd data, the certificates and keys of the `pki`
directory (see [Run Directory](stability.md#run-directory)), the encryption
config and the settings of `kubernix.toml`. Run directories created by an
incompatible KuberNix version are rejected and have to be removed. The
configuration files of the components get rendered on every start, so changes to
them get overwritten. Every component directory contains a `run.json`, which
records the command line the component got started with. For example, for etcd:

```text
> cat kubernix-run/etcd/run.json
```

```json
{
  "command": "/nix/store/…-etcd-…/bin/etcd",
  "args": [
    "--auto-compaction-mode=periodic",
    "--auto-compaction-retention=1h",
    "--client-cert-auth",
    "…",
    "--advertise-client-urls=https://127.0.0.1:2379",
    "--cert-file=/…/kubernix-run/pki/etcd.pem",
    "--data-dir=/…/kubernix-run/etcd/run",
    "…"
  ]
}
```

The generated Nix environment in the `nix` directory gets regenerated if it
differs from the one of the running KuberNix version, for example after an
upgrade or when the content of the overlay changed. The evaluated environment is
cached as a Nix profile in `nix-profile/<hash>`, where the hash covers the files
of the `nix` directory. This includes all inputs, since the overlay gets copied
into it and cannot refer to other local files (see
[Overlays](configuration.md#overlays)). Starting KuberNix or a new shell
re-enters the cached environment without evaluating the flake again, and a
changed or unusable environment, for example one with removed store paths, gets
replaced. The profile is a garbage collector root as well, so
`nix-collect-garbage` keeps the store paths of the run directory until it gets
removed.

The `kubernix.toml` keeps the settings of the existing cluster. KuberNix writes
it once the settings of a new run directory are valid, and removes it again if
the Nix environment cannot be created, for example for a package which does not
exist. Options which shape the cluster (`--nodes`, `--cidr`, `--cri-runtime`,
`--oci-runtime`, `--container-runtime` and the rootless mode) cannot be changed
for an existing run directory, so remove it to apply different values. The
subcommands ignore these options and always use the existing cluster. Other
options like `--log-level`, `--shell` or `--packages` override the stored values
if they are set on the command line or via the environment, and an empty value
like `--overlay=` or `KUBERNIX_PACKAGES=` overrides a stored overlay, Dockerfile
or package list with none.
