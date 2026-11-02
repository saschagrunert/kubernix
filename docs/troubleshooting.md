# Troubleshooting

KuberNix checks the prerequisites of rootless mode before it starts and fails
with one error per unmet prerequisite. The common causes and their fixes are:

- **No systemd user session**: `systemd-run --user` needs the bus of the systemd
  user instance (`$XDG_RUNTIME_DIR/bus` or `$DBUS_SESSION_BUS_ADDRESS`),
  otherwise it fails with `Failed to connect to bus`. Shells started via `su` or
  `sudo -u` have none, so log in via a regular session, for example `ssh` or
  `machinectl shell <user>@`. Alternatively, enable lingering via
  `loginctl enable-linger <user>` and set `XDG_RUNTIME_DIR=/run/user/$(id -u)`.
- **No subordinate IDs**: add a range for the user to `/etc/subuid` and
  `/etc/subgid`, for example via
  `sudo usermod --add-subuids 100000-165535 --add-subgids 100000-165535 <user>`.
  At least 65536 IDs are recommended. Subordinate IDs provided by NSS (the
  `subid` entry of `/etc/nsswitch.conf`) are supported, KuberNix verifies them
  via `getsubids` if available.
- **Missing `newuidmap` or `newgidmap`**: install them from the host
  distribution (`uidmap` on Debian and Ubuntu, `shadow-utils` on Fedora,
  `shadow` on Arch Linux). They need to be setuid root or have the `CAP_SETUID`
  or `CAP_SETGID` file capability, which Nix cannot provide.
- **User namespaces restricted**: enable them via
  `sudo sysctl -w kernel.unprivileged_userns_clone=1` (Debian based kernels) or
  `sudo sysctl -w user.max_user_namespaces=28633`. Ubuntu 23.10+ restricts them
  via AppArmor for binaries without a profile, like `rootlesskit` from the Nix
  store. Lift the restriction via
  `sudo sysctl -w kernel.apparmor_restrict_unprivileged_userns=0`, or write an
  AppArmor profile which allows `userns` for it. Add the settings to
  `/etc/sysctl.d` to make them persistent.
- **cgroup v1**: boot with `systemd.unified_cgroup_hierarchy=1` on the kernel
  command line to use the unified cgroup v2 hierarchy.
- **cgroup controllers not delegated**: KuberNix warns about every missing
  controller of `memory`, `cpu`, `pids` and `io`, without them pods may fail to
  start or their limits may not apply. Many distributions only delegate `memory`
  and `pids` to the user. To delegate all of them, create
  `/etc/systemd/system/user@.service.d/delegate.conf` with the content below,
  then run `sudo systemctl daemon-reload` and log in again:

  ```ini
  [Service]
  Delegate=cpu cpuset io memory pids
  ```

- **Port 6443 in use**: the rootless API server gets published on
  `127.0.0.1:6443`, which is not configurable yet. Stop the process using it,
  for example the cluster of another run directory, which
  `kubernix status --root <dir>` shows and `kubernix stop --root <dir>` stops.
- **`Permission denied` when removing the run directory**: it contains files
  owned by the subordinate IDs of the user, see [Cleanup](usage.md#cleanup) for
  how to remove it.
- **Proxy on the loopback interface**: the rootless network namespace cannot
  reach the loopback interface of the host, so a `HTTP_PROXY` or `HTTPS_PROXY`
  pointing to `localhost` or `127.0.0.1` does not work for image pulls and
  KuberNix warns about it. Let the proxy listen on another address of the host
  and use that one instead.
- **Pods cannot reach the API server or CoreDNS in root mode**: pods reach them
  via the node IP on the bridge (like `10.10.0.1`), so a firewall on the host
  which filters the `INPUT` chain, like the default firewall of NixOS,
  `firewalld` or `ufw`, may drop the traffic. Allow the incoming traffic of the
  bridges, which are named `kx<id>.<number>`, for example on NixOS via
  `networking.firewall.trustedInterfaces = [ "kx+" ];` with the default iptables
  backend or `[ "kx*" ]` with `networking.nftables.enable = true` (nftables
  matches a `+` literally), with `firewalld` via
  `sudo firewall-cmd --zone=trusted --add-interface=kx+` or with `ufw` via
  `sudo ufw allow in on kx+`. `ip link show type bridge` lists the bridges of
  the running cluster.
