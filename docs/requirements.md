# Requirements

There is only one real dependency needed to run KuberNix: **Nix**. To set up
Nix, run:

```shell
$ curl -L https://nixos.org/nix/install | sh
```

Please make sure to follow the instructions output by the script. KuberNix uses
[flakes][1] and enables the `nix-command` and `flakes` experimental features for
its own Nix invocations, so they do not have to be enabled in the Nix
configuration, also not for root.

[1]: https://wiki.nixos.org/wiki/Flakes

Besides that, KuberNix expects a Linux host (`x86_64` or `aarch64`). All cluster
components and tools come from Nix.

KuberNix runs in **root mode** when started as root, for example via `sudo`:

- `nix` has to be in the `PATH` of root, so run it via
  `sudo env "PATH=$PATH" kubernix`.
- `sudo` drops the proxy variables (`http_proxy`, `https_proxy`, `no_proxy` and
  their upper case variants) by default. If Nix or the nodes need a proxy, keep
  them via `sudo -E env "PATH=$PATH" kubernix` or `Defaults env_keep` in the
  sudoers configuration.
- The kernel modules `overlay`, `br_netfilter` and `ip_conntrack` get loaded and
  the required sysctls get set automatically, and restored when the cluster
  stops (see [Networking](configuration.md#networking)). Inside a container or a
  user namespace this step is skipped, so run
  [`contrib/prepare-system`](../contrib/prepare-system) on the host instead.
- Root in a user namespace, like in a rootless `podman run --privileged`
  container, an unprivileged LXC container or with Docker `userns-remap`, runs
  in root mode as well, except in the shell of a rootless cluster.
- The run directory and all its parent directories have to belong to root or to
  the user running KuberNix via `sudo` (`SUDO_UID`). None of them may be
  writable by others, or by their group, unless the group is the private group
  of the owner (the primary group of the owner with the same name and no other
  members, like with the user private groups of Fedora or RHEL; no other user in
  `/etc/passwd` may have it as primary group, users of other NSS sources like
  LDAP are not checked). Parent directories with the sticky bit (like `/tmp`)
  may be writable by group and others. Otherwise another user could plant files
  or symbolic links in the run directory or replace it, which root would write
  to or execute, so KuberNix refuses to use it. A new run directory gets the
  mode `0755`, so that the invoking user can access the admin kubeconfig.
- As root without `sudo` (no `SUDO_UID`), for example via `su -`, `doas`, an SSH
  login as root or inside a container with a mounted directory of a user, only
  directories of root are trusted. So a run directory below the home directory
  or a checkout of a user gets refused. Run KuberNix via `sudo` as that user
  instead, or use a run directory of root, for example
  `--root /var/lib/kubernix`.

Otherwise, it runs in **rootless mode**, which requires:

- Unprivileged user namespaces.
- cgroup v2 (the unified hierarchy) with the `memory`, `cpu`, `pids` and `io`
  controllers delegated to the user.
- The helpers `newuidmap` and `newgidmap` on the host, setuid root or with file
  capabilities, and subordinate IDs for the user. Nix cannot provide setuid
  binaries.
- `systemd-run` with a systemd user session, since KuberNix runs itself via
  `systemd-run --user --scope`.
- The port `6443` on `127.0.0.1` being free, since the API server gets published
  there. It is not configurable yet, so only one rootless cluster can run at a
  time.

KuberNix checks these prerequisites before starting the rootless session and
points to [Troubleshooting](troubleshooting.md), which describes how to fulfill
them, if one of them is not met.
