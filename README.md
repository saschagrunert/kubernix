<picture>
  <source media="(prefers-color-scheme: dark)" srcset=".github/logo-dark.png">
  <img src=".github/kubernix.png" width="300px" alt="KuberNix">
</picture>

[![CI](https://github.com/saschagrunert/kubernix/actions/workflows/ci.yml/badge.svg)](https://github.com/saschagrunert/kubernix/actions/workflows/ci.yml)
[![codecov](https://codecov.io/gh/saschagrunert/kubernix/graph/badge.svg)](https://codecov.io/gh/saschagrunert/kubernix)
[![Docs main](https://img.shields.io/badge/doc-main-orange.svg)](https://saschagrunert.github.io/kubernix/kubernix/index.html)
[![Docs release](https://docs.rs/kubernix/badge.svg)](https://docs.rs/kubernix)
[![Dependencies](https://deps.rs/repo/github/saschagrunert/kubernix/status.svg)](https://deps.rs/repo/github/saschagrunert/kubernix)
[![Crates.io](https://img.shields.io/crates/v/kubernix.svg)](https://crates.io/crates/kubernix)
[![License Apache-2.0](https://img.shields.io/badge/license-Apache--2.0-blue.svg)](https://github.com/saschagrunert/kubernix/blob/main/LICENSE)

## Kubernetes development cluster bootstrapping with Nix packages

This project aims to provide **single dependency** [Kubernetes][1] clusters for
local testing, experimenting and development purposes.

[1]: https://kubernetes.io

Moving pictures are worth more than a thousand words, so here is a short demo:

![demo](.github/kubernix.gif)

### Nix?

Have you ever heard about [Nix][2], the functional package manager?

In case you haven’t, don’t worry, the important thing is that it provides all
the third-party dependencies needed for this project, pinned to a dedicated
version. This guarantees stable, reproducible installations.

[2]: https://nixos.org

KuberNix itself is a Rusty helper program, which takes care of bootstrapping the
Kubernetes cluster, passing the right configuration parameters around and
keeping track of the running processes.

### What is inside

The following technology stack is currently being used:

| Application     | Version  |
| --------------- | -------- |
| cni-plugins     | v1.9.1   |
| conmon-rs       | v1.1.1   |
| conntrack-tools | v1.4.9   |
| containerd      | v2.4.1   |
| cri-o-wrapper   | v1.37.2  |
| cri-tools       | v1.37.0  |
| crun            | v1.30.1  |
| etcd            | v3.7.2   |
| iproute2        | v7.2.0   |
| iptables        | v1.8.13  |
| kmod            | v31      |
| kubectl         | v1.37.1  |
| kubernetes      | v1.37.1  |
| nss-cacert      | v3.131.1 |
| podman          | v5.8.9   |
| rootlesskit     | v2.3.6   |
| runc            | v1.5.2   |
| slirp4netns     | v1.3.6   |
| socat           | v1.8.1.3 |
| util-linux      | v2.42.4  |

Some other tools are not explicitly mentioned here, because they are not direct
dependencies.

### Quick Start

Install [Nix][2], the only real dependency:

```shell
$ curl -L https://nixos.org/nix/install | sh
```

Then download the static binary for your architecture (`kubernix-x86_64` or
`kubernix-aarch64`) from the [latest release][3], install it via
`cargo install kubernix` or build it via `make build-release`, and start
KuberNix as a regular user (rootless mode) or as root:

[3]: https://github.com/saschagrunert/kubernix/releases/latest

```console
$ kubernix
$ sudo env "PATH=$PATH" kubernix
```

KuberNix sets up the Nix environment, starts the cluster with all its data in
the `kubernix-run` directory and drops you into a shell where `kubectl` talks to
the cluster. Exiting the shell stops the cluster. The
[requirements](docs/requirements.md) list the prerequisites of both modes.

### Documentation

<!-- The anchors keep links to the former README sections working. -->

- <a id="requirements"></a>[Requirements](docs/requirements.md): Nix as well as
  the prerequisites of root and rootless mode.
- <a id="getting-started"></a>[Usage](docs/usage.md): bootstrapping, shells,
  status, stopping, cleanup and restarts.
- <a id="configuration"></a>[Configuration](docs/configuration.md): options, OCI
  runtimes, multinode clusters, networking, overlays and additional packages.
- <a id="boot-phases"></a>[Architecture](docs/architecture.md): the boot phases
  of the components.
- <a id="troubleshooting"></a>[Troubleshooting](docs/troubleshooting.md): fixes
  for unmet rootless prerequisites and other common problems.
- <a id="stability"></a><a id="upgrading-from-04"></a>[Stability](docs/stability.md):
  the stable interfaces since 1.0 and upgrading from 0.4.
- <a id="verifying-releases"></a>[Verifying Releases](docs/releases.md):
  checksums, signatures, SBOMs, attestations and vulnerability exploitability
  (VEX) statements.

## Contributing

You want to contribute to this project? Wow, thanks! So please just fork it and
send me a pull request. The `build` dev shell of the flake provides all tools
needed for development, for example via `nix develop .#build`.
