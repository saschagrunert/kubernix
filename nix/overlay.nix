# Nix package overlay for kubernix.
#
# This file is applied to the nixpkgs set used by both the development
# shell (flake.nix) and the runtime environment (runtime-flake.nix).
# It is intentionally empty by default.
#
# To override or add packages, use standard overlay syntax. For example,
# to pin a specific version of etcd:
#
#   self: super: {
#     etcd = super.etcd.overrideAttrs (old: rec {
#       version = "3.5.0";
#       src = super.fetchFromGitHub { ... };
#     });
#   }
#
# You can also pass your own overlay file at runtime with --overlay.
self: super: { }
