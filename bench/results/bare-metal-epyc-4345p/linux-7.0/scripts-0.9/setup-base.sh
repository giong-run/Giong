#!/bin/bash
# Base setup of a fresh Vultr EPYC 4345P box for the giong bare-metal runs.
# No NIC or boot change takes effect here: the HWE kernel is installed but the
# default boot entry stays on the stock 6.8 kernel; a later one-shot
# grub-reboot tries 7.0 (a power cycle falls back to 6.8).
set -euo pipefail
export DEBIAN_FRONTEND=noninteractive
exec >>/root/setup-base.log 2>&1
echo "== $(date -u +%FT%TZ) start on $(hostname) $(uname -r)"
# Pin the default boot entry to the running stock kernel before any kernel install.
sed -i 's/^GRUB_DEFAULT=.*/GRUB_DEFAULT=saved/' /etc/default/grub
uuid=$(grep -o "gnulinux-advanced-[0-9a-f-]*" /boot/grub/grub.cfg | head -1 | sed 's/gnulinux-advanced-//')
grub-set-default "gnulinux-advanced-$uuid>gnulinux-$(uname -r)-advanced-$uuid"
echo "kernel.panic = 10" >/etc/sysctl.d/90-panic-reboot.conf; sysctl -q -p /etc/sysctl.d/90-panic-reboot.conf
apt-get update -q
apt-get install -y -q build-essential pkg-config git curl ca-certificates wrk iperf3 nodejs npm nginx nghttp2-client golang-go \
  python3 ethtool linux-tools-common linux-tools-$(uname -r) valgrind unzip jq
apt-mark manual linux-image-$(uname -r)
apt-get install -y -q linux-generic-hwe-24.04
update-grub
apt-get install -y -q linux-tools-generic-hwe-24.04 || true
cat >/etc/security/limits.d/90-nofile.conf <<'L'
root soft nofile 1048576
root hard nofile 1048576
L
[ -x /root/.cargo/bin/rustc ] || curl -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal
. /root/.cargo/env
rustup component add rustfmt clippy
[ -x /root/.bun/bin/bun ] || (curl -fsSL https://bun.sh/install | bash)
[ -d /root/giong-main/.git ] || git clone -q https://github.com/giong-run/Giong.git /root/giong-main
echo "== $(date -u +%FT%TZ) done; default entry: $(grub-editenv list)"
ls /boot/vmlinuz-*
