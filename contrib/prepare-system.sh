#!/usr/bin/env bash
set -euo pipefail

# Needs to be set if running this project inside a container
modprobe overlay
modprobe ip_conntrack
modprobe br_netfilter
sysctl net.bridge.bridge-nf-call-ip6tables=1
sysctl net.bridge.bridge-nf-call-iptables=1
sysctl net.ipv4.conf.all.route_localnet=1
sysctl net.ipv4.ip_forward=1
