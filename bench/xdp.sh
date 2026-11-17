#!/usr/bin/env bash
# Small-packet UDP echo on one server core: kernel sockets (recvmmsg/sendmmsg),
# giong, Tokio, and an AF_XDP path. See xdp/src/main.rs for the method.
#
#   sudo bench/xdp.sh [rounds] [seconds]
#
# MODES picks the servers (kernel giong giong-epoll tokio xdp), TASKS the echo
# tasks per giong or Tokio server (16 by default; 1 is a single receive loop).
#
# Needs root (network namespace, veth, XDP program), iproute2 and a kernel
# with CONFIG_XDP_SOCKETS. CPU 1 runs the server, CPUs 2-3 the client.
# Results: bench/results/xdp/<host>/.
set -euo pipefail
cd "$(dirname "$0")"

ROUNDS=${1:-5}
SECS=${2:-8}
MODES=${MODES:-"kernel giong tokio xdp"}
OUT=${OUT:-results/xdp/$(hostname -s)}
SERVER_CPU=${SERVER_CPU:-1}
CLIENT_CPUS=${CLIENT_CPUS:-2-3}
CLIENT_THREADS=${CLIENT_THREADS:-2}
mkdir -p "$OUT"

# BIN: a bench-xdp built elsewhere (for instance against another giong
# revision); by default the one built here.
if [ -z "${BIN:-}" ]; then
    (cd xdp && cargo build --release -q)
    BIN=$PWD/xdp/target/release/bench-xdp
fi

ip netns del xc 2>/dev/null || true
ip link del xs 2>/dev/null || true
ip netns add xc
ip link add xs type veth peer name xcl
ip link set xcl netns xc
ip addr add 10.77.0.1/24 dev xs
ip -n xc addr add 10.77.0.2/24 dev xcl
ip link set xs up
ip -n xc link set xcl up
ip -n xc link set lo up
# The XDP program takes every frame, ARP included: fixed neighbours.
mac_s=$(cat /sys/class/net/xs/address)
mac_c=$(ip netns exec xc cat /sys/class/net/xcl/address)
ip neigh replace 10.77.0.2 lladdr "$mac_c" dev xs nud permanent
ip -n xc neigh replace 10.77.0.1 lladdr "$mac_s" dev xcl nud permanent
trap 'ip netns del xc 2>/dev/null; ip link del xs 2>/dev/null; true' EXIT

# "<process ticks> <softirq ticks on SERVER_CPU>"
server_ticks() {
    local p
    p=$(awk '{print $14 + $15}' "/proc/$1/stat")
    echo "$p $(awk -v c="cpu$SERVER_CPU" '$1 == c {print $8}' /proc/stat)"
}

CSV="$OUT/raw.csv"
{
    echo "# $(date -u +%FT%TZ) $(uname -r) $(nproc) CPUs: $(grep -m1 'model name' /proc/cpuinfo | cut -d: -f2 | xargs); veth, ${PAYLOAD:-64}-byte payload, ${TASKS:-16} tasks"
    echo "round,mode,echoes_per_s,machine_cpu_us_per_echo,server_process_us_per_echo,server_cpu_softirq_us_per_echo"
} > "$CSV"
for round in $(seq 1 "$ROUNDS"); do
    for m in $MODES; do
        "$BIN" server "$m" xs "$SERVER_CPU" 2> "$OUT/server-$m.log" &
        spid=$!
        sleep 1
        s0=$(server_ticks "$spid")
        line=$(ip netns exec xc taskset -c "$CLIENT_CPUS" "$BIN" client 10.77.0.1 "$CLIENT_THREADS" "$SECS")
        s1=$(server_ticks "$spid")
        # Server CPU per echo: its process time plus softirq time on its CPU,
        # over the whole client run (warm-up second + measured seconds).
        line="$line,$(python3 -c "
e=float('${line%%,*}'); a=[int(x) for x in '$s0'.split()]; b=[int(x) for x in '$s1'.split()]
print(f'{(b[0]-a[0])/100/(e*($SECS+1))*1e6:.3f},{(b[1]-a[1])/100/(e*($SECS+1))*1e6:.3f}')")"
        kill -9 "$spid" 2>/dev/null || true; wait "$spid" 2>/dev/null || true; pkill -9 -f "bench-xdp server" || true; sleep 2  # the kernel releases an AF_XDP socket late
        echo "round $round: $m $line" >&2
        echo "$round,$m,$line" >> "$CSV"
    done
done

python3 - "$CSV" <<'PY' | tee "$OUT/summary.txt"
import csv, statistics, sys
rows = [r for r in csv.reader(l for l in open(sys.argv[1]) if not l.startswith('#'))][1:]
by, order = {}, []
for r in rows:
    by.setdefault(r[1], []).append([float(x) for x in r[2:]])
    if r[1] not in order: order.append(r[1])
print(open(sys.argv[1]).readline().strip())
cols = ["echoes/s", "machine CPU us/echo", "server process us/echo", "server-CPU softirq us/echo"]
print(f"{'mode':<8}" + "".join(f"{c:>30}" for c in cols))
for m in order:
    cells = []
    for i in range(len(cols)):
        xs = [v[i] for v in by[m]]
        f = "{:.0f}" if i == 0 else "{:.3f}"
        cells.append((f + " (" + f + "-" + f + ")").format(statistics.median(xs), min(xs), max(xs)))
    print(f"{m:<8}" + "".join(f"{c:>30}" for c in cells))
print(f"rounds: {len(by[order[0]])}")
PY
