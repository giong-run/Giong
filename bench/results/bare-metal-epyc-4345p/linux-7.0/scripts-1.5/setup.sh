#!/bin/bash
# Base setup of a fresh Vultr EPYC 4345P box (Ubuntu 24.04) for the 1.5.x bare-metal run.
# Run as root on each machine: bash setup.sh. Safe to run again.
#
# No boot or NIC change takes effect here: the HWE kernel is installed, but the default boot
# entry is pinned to the running kernel; boot7.sh tries the new one once (grub-reboot), and
# a panic or a soft lockup reboots to the default entry.
set -euo pipefail
export DEBIAN_FRONTEND=noninteractive
mkdir -p /root/r15
exec > >(tee -a /root/r15/setup.log) 2>&1
echo "== $(date -u +%FT%TZ) setup on $(hostname) $(uname -r)"

# Pin the default boot entry to the running kernel before any kernel install.
sed -i 's/^GRUB_DEFAULT=.*/GRUB_DEFAULT=saved/' /etc/default/grub
uuid=$(grep -o "gnulinux-advanced-[0-9a-f-]*" /boot/grub/grub.cfg | head -1 | sed 's/gnulinux-advanced-//')
grub-set-default "gnulinux-advanced-$uuid>gnulinux-$(uname -r)-advanced-$uuid"
cat >/etc/sysctl.d/90-r15.conf <<'C'
kernel.panic = 10
kernel.panic_on_oops = 1
kernel.softlockup_panic = 1
C
# Network sysctls stay at Ubuntu's defaults, as in the 0.9 runs this one is compared with.
sysctl -q -p /etc/sysctl.d/90-r15.conf

apt-get update -q
apt-get install -y -q build-essential pkg-config cmake clang git curl ca-certificates wrk iperf3 nghttp2-client \
  golang-go python3 ethtool linux-tools-common "linux-tools-$(uname -r)" "linux-modules-extra-$(uname -r)" \
  jq tmux libssl-dev zlib1g-dev sysstat iproute2 numactl net-tools
apt-mark manual "linux-image-$(uname -r)"
apt-get install -y -q linux-generic-hwe-24.04 linux-tools-generic-hwe-24.04 || true
update-grub
grub-set-default "gnulinux-advanced-$uuid>gnulinux-$(uname -r)-advanced-$uuid"

cat >/etc/security/limits.d/90-nofile.conf <<'L'
root soft nofile 1048576
root hard nofile 1048576
root soft memlock unlimited
root hard memlock unlimited
L

[ -x /root/.cargo/bin/rustup ] || curl -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal --default-toolchain 1.97.0
. /root/.cargo/env
rustup toolchain install 1.97.0 --profile minimal
rustup default 1.97.0

# wrk2 (constant-rate wrk) for the gzip latency run; the rate sweeps use bench/lab's loadgen.
if ! command -v wrk2 >/dev/null; then
  (rm -rf /root/wrk2 && git clone -q https://github.com/giltene/wrk2 /root/wrk2 && make -C /root/wrk2 -j8 >/root/r15/wrk2-build.log 2>&1 \
    && cp /root/wrk2/wrk /usr/local/bin/wrk2) || echo "wrk2 did not build (see /root/r15/wrk2-build.log); gzip.sh falls back to wrk"
fi

[ -d /root/giong/.git ] || git clone -q https://github.com/giong-run/Giong.git /root/giong
[ -f /root/.ssh/id_ed25519 ] || ssh-keygen -q -t ed25519 -N "" -f /root/.ssh/id_ed25519
grep -q "StrictHostKeyChecking" /root/.ssh/config 2>/dev/null || printf 'Host 10.* 192.168.* 172.*\n  StrictHostKeyChecking accept-new\n' >>/root/.ssh/config

{
  echo "host $(hostname), kernel $(uname -r), default entry: $(grub-editenv list)"
  grep -m1 "model name" /proc/cpuinfo; nproc; free -g | head -2
  lscpu | grep -E "Thread|Core|Socket|NUMA|MHz"
  ip -br -4 addr
  for i in $(ls /sys/class/net | grep -v lo); do echo "$i: $(ethtool -i "$i" 2>/dev/null | grep -E '^driver|^version' | tr '\n' ' ') $(ethtool -l "$i" 2>/dev/null | grep -A4 Current | grep Combined)"; done
  echo "rustc: $(rustc -V); wrk: $(wrk -v 2>&1 | head -1); h2load: $(h2load --version 2>&1 | head -1); wrk2: $(command -v wrk2 || echo none)"
  ls /boot/vmlinuz-*
} >/root/r15/machine-$(hostname).txt 2>&1
echo "== $(date -u +%FT%TZ) done; public key for the other machine:"
cat /root/.ssh/id_ed25519.pub
