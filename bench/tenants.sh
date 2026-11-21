#!/usr/bin/env bash
# Two tenants on one small box: does tenant B keep its in-deadline successes
# while tenant A sends far more than the server can do? See
# tenants/src/bin/tenants-server.rs for the server and tenants-load.rs for
# the load.
#
#   sudo bench/tenants.sh [rounds] [seconds]
#
# Method
#   * The server runs in a cgroup limited to 1 CPU (cpu quota = period) and
#     128 MiB of memory, pinned to CPU 0, with 1 worker and 1 compute thread.
#     The load runs on the other CPUs, outside the cgroup.
#   * Every request holds MEM_KIB of memory from its start to its answer and
#     runs WORK_US of CPU on the compute lane.
#   * Tenant B sends B_RATE requests/s open loop on B_CONNS connections; a
#     success is a 200 within DEADLINE_MS of the request's scheduled time.
#     Tenant A, in the `overload` runs, keeps A_CONNS requests in flight back
#     to back: more than the box can do.
#   * `shared` is the same service without budgets (one FIFO compute queue,
#     no limits); `isolated` gives each tenant a Budget (TENANT_MEM_MIB of
#     memory, TENANT_COMPUTE compute closures), with the waiting forms only.
#   * Scenarios: `cpu` (WORK_US=2000, MEM_KIB=256, A_CONNS=64) and `memory`
#     (WORK_US=500, MEM_KIB=2048, A_CONNS=128: A alone would hold 256 MiB).
#   * Acceptance (2026-11-20): with `isolated`, on both drivers, B's
#     in-deadline successes under overload are >= 90% of its solo ones.
#
# Requires: root (cgroups), cargo, python3, taskset. cgroup v1 or v2.
# Results: bench/results/tenants/.
set -euo pipefail
cd "$(dirname "$0")"

ROUNDS=${1:-3}
SECS=${2:-20}
OUT=${OUT:-results/tenants}
DRIVERS=${DRIVERS:-"io_uring epoll"}
MODES=${MODES:-"shared isolated"}
SCENARIOS=${SCENARIOS:-"cpu memory"}
B_RATE=${B_RATE:-100}
B_CONNS=${B_CONNS:-10}
DEADLINE_MS=${DEADLINE_MS:-50}
MEM_LIMIT=${MEM_LIMIT:-$((128 << 20))}
mkdir -p "$OUT"
CSV="$OUT/raw.csv"

echo "building..." >&2
(cd tenants && cargo build --release -q)
SERVER=tenants/target/release/tenants-server
LOAD=tenants/target/release/tenants-load

# One cgroup for the server, v2 or v1.
CG=giong-tenants
if [ -f /sys/fs/cgroup/cgroup.controllers ]; then
    V=2
    D=/sys/fs/cgroup/$CG
    mkdir -p "$D"
    echo "100000 100000" > "$D/cpu.max"
    echo "$MEM_LIMIT" > "$D/memory.max"
    echo 0 > "$D/memory.swap.max" 2>/dev/null || true
    procs="$D/cgroup.procs"
else
    V=1
    DC=/sys/fs/cgroup/cpu/$CG
    DM=/sys/fs/cgroup/memory/$CG
    mkdir -p "$DC" "$DM"
    echo 100000 > "$DC/cpu.cfs_period_us"
    echo 100000 > "$DC/cpu.cfs_quota_us"
    echo "$MEM_LIMIT" > "$DM/memory.limit_in_bytes"
    procs="$DC/cgroup.procs $DM/cgroup.procs"
fi

peak_bytes() {
    if [ $V = 2 ]; then cat "$D/memory.peak" 2>/dev/null || echo 0
    else cat "$DM/memory.max_usage_in_bytes"; fi
}
reset_peak() {
    if [ $V = 2 ]; then echo 0 > "$D/memory.peak" 2>/dev/null || true
    else echo 0 > "$DM/memory.max_usage_in_bytes"; fi
}
oom_kills() {
    if [ $V = 2 ]; then awk '/^oom_kill /{print $2}' "$D/memory.events"
    else awk '/^oom_kill /{print $2}' "$DM/memory.oom_control"; fi
}

scenario_env() {
    case $1 in
        cpu) echo "WORK_US=2000 MEM_KIB=256 A_CONNS=64" ;;
        memory) echo "WORK_US=500 MEM_KIB=2048 A_CONNS=128" ;;
    esac
}

{
    echo "# $(date -u +%FT%TZ) $(uname -r) $(nproc) CPUs: $(grep -m1 'model name' /proc/cpuinfo | cut -d: -f2 | xargs); cgroup v$V; B ${B_RATE}/s on $B_CONNS conns, deadline ${DEADLINE_MS} ms, ${SECS} s"
    echo "round,scenario,driver,mode,load,b_sent,b_ok_in_deadline,b_ok_late,b_err,b_p50_ms,b_p99_ms,a_ok,a_err,peak_mib,oom_kills,server_alive"
} > "$CSV"

port=9800
for round in $(seq 1 "$ROUNDS"); do
for scenario in $SCENARIOS; do
for driver in $DRIVERS; do
for mode in $MODES; do
for load in solo overload; do
    port=$((port + 1))
    env_line=$(scenario_env "$scenario")
    a_conns=0
    if [ $load = overload ]; then a_conns=$(echo "$env_line" | sed 's/.*A_CONNS=//'); fi
    reset_peak
    kills_before=$(oom_kills)
    # The shell joins the cgroup, then becomes the server.
    sh -c "for p in $procs; do echo \$\$ > \$p; done; exec env $env_line GIONG_DRIVER=$driver taskset -c 0 $SERVER 127.0.0.1:$port $mode" \
        2> "$OUT/server.log" &
    pid=$!
    for _ in $(seq 100); do
        (exec 3<>/dev/tcp/127.0.0.1/$port) 2>/dev/null && break
        sleep 0.05
    done
    line=$(taskset -c 1-$(($(nproc) - 1)) "$LOAD" 127.0.0.1:$port "$SECS" "$B_RATE" "$B_CONNS" "$DEADLINE_MS" "$a_conns")
    alive=1
    kill -0 $pid 2>/dev/null || alive=0
    kill $pid 2>/dev/null || true
    wait $pid 2>/dev/null || true
    peak=$(( $(peak_bytes) >> 20 ))
    kills=$(( $(oom_kills) - kills_before ))
    echo "$round,$scenario,$driver,$mode,$load,$line,$peak,$kills,$alive" | tee -a "$CSV" >&2
done; done; done; done; done

python3 - "$CSV" <<'PY' | tee "$OUT/summary.txt"
import csv, statistics, sys
lines = open(sys.argv[1]).read().splitlines()
print(lines[0])
rows = list(csv.DictReader(l for l in lines if not l.startswith('#')))
key = lambda r: (r['scenario'], r['driver'], r['mode'])
groups = {}
for r in rows:
    groups.setdefault(key(r), {}).setdefault(r['load'], []).append(r)
print(f"{'scenario':<8}{'driver':<10}{'mode':<10}{'B solo ok':>11}{'B overload ok':>15}{'kept':>8}{'B p99 ms':>10}{'A ok':>9}{'peak MiB':>10}{'OOM':>5}")
for k in sorted(groups):
    g = groups[k]
    solo = statistics.median(int(r['b_ok_in_deadline']) for r in g.get('solo', []))
    over = g.get('overload', [])
    ov = statistics.median(int(r['b_ok_in_deadline']) for r in over)
    p99 = statistics.median(float(r['b_p99_ms']) for r in over)
    aok = statistics.median(int(r['a_ok']) for r in over)
    peak = max(int(r['peak_mib']) for r in over)
    ooms = sum(int(r['oom_kills']) for r in over)
    print(f"{k[0]:<8}{k[1]:<10}{k[2]:<10}{solo:>11.0f}{ov:>15.0f}{100*ov/solo if solo else 0:>7.1f}%{p99:>10.1f}{aok:>9.0f}{peak:>10}{ooms:>5}")
print("kept = B's in-deadline successes under overload / solo (medians over rounds); A ok = total over the run")
PY
