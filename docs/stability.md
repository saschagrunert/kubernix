# Stability

Starting with version 1.0, KuberNix follows [semantic versioning][1] for the
interfaces listed below. Breaking changes to them require a new major version,
while minor versions may add new options, subcommands, environment variables,
log fields or files.

[1]: https://semver.org

## Command Line

- The subcommands and their options, the long option names and the short options
  in the [configuration tables](configuration.md), as well as their value
  formats.
- The environment variables of the options (`KUBERNIX_*`), which take the same
  values as the options. Variables starting with `_KUBERNIX_` are internal and
  not part of the interface.
- The `-e`, `-f` and `-u` short options of earlier versions keep working as
  hidden aliases of `--no-shell`, `--log-format` and `--container-runtime`, but
  are no longer documented.

The versions of the bundled packages (see
[What is inside](../README.md#what-is-inside)) are not part of the stable
interface and get updated in any release.

## Exit Codes

| Code | Meaning                                                                                                  |
| ---- | -------------------------------------------------------------------------------------------------------- |
| `0`  | Success, for example after the interactive shell exited, on `SIGTERM` with `--no-shell` or after a stop  |
| `1`  | Any error, including a failed bootstrap, a dying component with `--no-shell` or an aborted bootstrap     |
| `2`  | Command line arguments or environment variables which cannot be parsed                                   |
| `3`  | `kubernix stop`, `kubernix status` and `kubernix shell` only: no cluster is running in the run directory |

`kubernix status` exits with `0` while a cluster is running or starting, and
`kubernix stop` with `0` once the cluster stopped, also if it aborted its
bootstrap. The aborted instance itself exits with a non-zero code.
`kubernix shell` replaces itself with the spawned shell or the command passed
after `--`, so it returns their exit code once they started. It exits with `1`
if the cluster is still starting. Subcommands may define further exit codes,
which are documented together with them.

## Environment File

The `kubernix.env` file in the run directory can be sourced by any POSIX shell.
Every line has the format `export KEY='value'`, where the value uses POSIX
single quoting. It contains at least these variables:

- `KUBECONFIG`: the path to the admin kubeconfig.
- `CONTAINER_RUNTIME_ENDPOINT`: the CRI socket of the first node, as
  `unix://<path>`.

## Status Output

`kubernix status --output json` prints one JSON object to stdout. Its field
names use kebab-case like the options and `kubernix.toml`:

- `root`: the absolute path of the run directory.
- `state`: one of `running`, `starting` or `stopped`.
- `pid`: the PID of the KuberNix instance, missing if stopped or unknown.

If the run directory contains a cluster, these fields are present as well:

- `mode`: `root` or `rootless`.
- `nodes`: the number of nodes.
- `cri-runtime`: `crio` or `containerd`.
- `oci-runtime`: `crun` or `runc`.
- `nri`: whether NRI is enabled (`--nri`).
- `api-server`: the URL of the API server.
- `node-status`: only while running and if the API server answered, a list of
  objects with the node `name` and whether it is `ready`.

Consumers should ignore unknown fields.

## JSON Logs

With `--log-format=json`, KuberNix writes one JSON object per line to stderr
with the following fields:

- `level`: one of `error`, `warn`, `info`, `debug` or `trace`.
- `message`: the log message, which is meant for humans and may change.
- `target`: the source of the message, which is meant for debugging and may
  change.

Consumers should ignore unknown fields. The error which terminates KuberNix is
written as one `error` object, also with `--log-level=off`, whose message
contains its causes separated by a colon and a space. Errors which occur before
the logger is set up, for example in an unusable `kubernix.toml`, are written as
plain text lines (`[ERROR] …`). Command line arguments and environment variables
which cannot be parsed (exit code `2`) are reported in the format of the
argument parser instead.

## Run Directory

The run directory (`--root`) keeps these files at stable locations:

- `kubernix.toml`: the configuration of the cluster, see below.
- `kubernix.env`: the environment file described above.
- `kubeconfig/admin.kubeconfig`: the kubeconfig of the cluster admin.
- `kubernix.pid`: the PID of the running KuberNix instance, written once the
  cluster is ready and removed after the cleanup.
- The component log files listed in
  [Shell Environment](usage.md#shell-environment).
- The NRI sockets and plugin directories listed in [NRI](configuration.md#nri).

All other files, like the PKI, the component configurations and the generated
Nix environment, are internal and may change in any release.

KuberNix reuses the certificates and keys of the `pki` directory if they are
valid and match the cluster, and regenerates the affected ones otherwise, for
example the serving certificates of the API server and of the kubelet in single
node mode after the hostname changed. The serving certificates do not depend on
the network of the host: in root mode, they contain the node IPs, which are the
bridge gateways (the API server only the one of the first node, which it
advertises), and in rootless mode, the kubelet certificates contain the fixed
addresses of the network namespace of rootlesskit. Once etcd contains data, it
refuses to regenerate the cluster CA, the ServiceAccount key and the encryption
config, since the stored cluster state depends on them. Restore them in this
case, or remove the run directory.

The `kubernix.toml` contains a `version` field, which is the format version of
the run directory. A run directory without it has version 0. KuberNix uses run
directories of the same version as is, migrates those of older versions in place
when starting a cluster if possible, and otherwise refuses them with an error
which contains the command to remove the run directory (see
[Cleanup](usage.md#cleanup)). It also refuses run directories created by a newer
KuberNix, starting with 1.0: earlier versions ignore the `version` field and use
any run directory. Version 2 changed the network layout and the PKI, so run
directories of version 0 and 1 have to be removed, and KuberNix versions before
1.0 must not be used with run directories of version 2. Version 3 adds the `nri`
setting (`--nri`), which KuberNix 1.0.0 would ignore. Only run directories with
NRI enabled get version 3, so that KuberNix 1.0.0 refuses them, while all other
run directories keep version 2 and work with both. The other fields of the file
are not part of the stable interface.

## Library

The `kubernix` crate only exists to build the command line tool. Its library
target is not a public API, is hidden from the documentation and is not covered
by semantic versioning.

## Upgrading from 0.4

- Run directories of 0.4 cannot be used anymore, since the network layout and
  the PKI changed. Remove them before starting a new cluster, via
  `sudo rm -rf kubernix-run` for a root cluster or
  `unshare --map-auto --map-root-user rm -rf kubernix-run` for a rootless one
  (see [Cleanup](usage.md#cleanup)).
- The short options `-e`, `-f` and `-u` are hidden aliases now, use
  `--no-shell`, `--log-format` and `--container-runtime` instead.
- Options which take a list (`--packages` and `--addons`) take comma separated
  or repeated values, like `-p jq,kubernetes-helm` or
  `-p jq -p kubernetes-helm`, but no space separated values anymore. The same
  applies to `KUBERNIX_PACKAGES` and `KUBERNIX_ADDONS`.
- `make run` and the other Makefile targets run KuberNix rootless by default,
  use `SUDO='sudo -E env "PATH=$$PATH"'` to run it as root (`$$` because make
  expands `$`).
- `cfssl` is not needed anymore, the certificates get generated by KuberNix
  itself.
- Release binaries only exist for `x86_64` and `aarch64`, the `armv7`,
  `ppc64le`, `riscv64` and `s390x` binaries are not built anymore.
- A custom `--dockerfile` uses the directory of the Dockerfile as build context.
  The node containers get the Nix store of the host mounted, so the image does
  not need to contain Nix (see
  [Multinode Support](configuration.md#multinode-support)). Container runtimes
  other than podman refuse a build context which contains run directories, so
  move a Dockerfile next to `kubernix-run` into its own directory.
