# Shared settings of the 1.5.x bare-metal scripts; sourced, not run.
# /root/r15/hosts holds the private addresses: S=<server, br-1> L=<load generator, br-2>.
set -u
[ -f /root/.cargo/env ] && . /root/.cargo/env
ulimit -n 1048576 2>/dev/null || ulimit -n "$(ulimit -Hn)"
export CARGO_NET_RETRY=10 CARGO_TERM_COLOR=never
R15=/root/r15
mkdir -p "$R15/out"
[ -f "$R15/hosts" ] && . "$R15/hosts"
S=${S:-} L=${L:-}
REPO=https://github.com/giong-run/Giong.git
B=/root/b # checkouts
mkdir -p "$B"
log() { echo "$(date -u +%FT%TZ) $*" | tee -a "$R15/progress.log" >&2; }
# co <dir> <ref or sha>: a detached checkout of the public repository.
co() {
  local d=$1 ref=$2
  [ -d "$d/.git" ] || git clone -q "$REPO" "$d"
  (cd "$d" && git fetch -q origin "$ref" && git checkout -q --detach -f FETCH_HEAD) || { log "checkout $ref into $d failed"; return 1; }
  log "checkout $d = $ref at $(git -C "$d" rev-parse --short HEAD), tree $(git -C "$d" rev-parse --short 'HEAD^{tree}')"
}
# on_l "<command line>": one string, run by the shell on br-2 (quote arguments with printf %q).
on_l() { ssh -o BatchMode=yes "root@$L" "$@"; }
# to_l <files...>: copies files to /root/ on br-2.
to_l() { scp -q "$@" "root@$L:/root/"; }
# R15_LOCAL=1: br-2 is this machine (a smoke test of the scripts on one box).
if [ -n "${R15_LOCAL:-}" ]; then
  on_l() { bash -c "$*"; }
  to_l() { local f; for f in "$@"; do [ "$(readlink -f "$f")" = "$(readlink -f "/root/$(basename "$f")")" ] || cp "$f" /root/; done; }
fi
# Busy and total jiffies over a set of CPUs ("all" for every CPU), from /proc/stat.
cpu_busy() {
  local cpus=" $* "
  awk -v cpus="$cpus" '$1 ~ /^cpu[0-9]+$/ { n = substr($1, 4); if (cpus == " all " || index(cpus, " " n " ")) {
      t = 0; for (i = 2; i <= NF; i++) t += $i; idle += $5 + $6; total += t } }
      END { print total - idle, total }' /proc/stat
}
ticks() { cat /proc/"$1"/task/*/stat 2>/dev/null | awk '{t += $14 + $15} END {print t + 0}'; }
HZ=$(getconf CLK_TCK)
performance() {
  for f in /sys/devices/system/cpu/cpu*/cpufreq/energy_performance_preference; do echo performance >"$f" 2>/dev/null; done
}
# The interface that carries an address.
iface_of() { ip -o -4 addr | awk -v a="$1" '{split($4, p, "/"); if (p[1] == a) print $2}'; }
nic_bytes() { [ -n "$1" ] && [ -d /sys/class/net/"$1" ] || { echo 0; return; }; echo $(( $(cat /sys/class/net/"$1"/statistics/rx_bytes) + $(cat /sys/class/net/"$1"/statistics/tx_bytes) )); }
