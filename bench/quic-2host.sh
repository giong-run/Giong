#!/usr/bin/env bash
# The QUIC decision benchmark across two machines (bare metal): the server
# on SERVER over the NIC, the client here. Same servers, client and engine
# as bench/quic.sh; adds per-core scaling (WORKERS) and larger GSO batches.
#
#   SERVER=root@10.0.0.2 SERVER_ADDR=10.0.0.2:4433 bench/quic-2host.sh [rounds] [secs]
#
# Needs: this repository at the same path on both machines (REPO), cargo on
# both, root ssh from here to SERVER without a password, UDP port 4433 open.
# Servers (SERVERS):
#   quinn          quinn 0.11 on Tokio: current-thread for 1 worker, the
#                  multi-threaded scheduler with W threads otherwise.
#   proto-tokio    quinn-proto engine on Tokio + quinn-udp (1 worker only).
#   giong          quinn-proto engine on giong with the offload prototype,
#                  one engine and socket per worker on the same port.
#   giong-seg44    giong with 44 datagrams per GSO send instead of 10.
#   giong-plain    giong without offloads (1 worker only).
#   giong-epoll    giong on its epoll driver.
#   giong-quic     the giong-quic crate, workers pinned (the default).
#   giong-quic-nopin  giong-quic with workers not pinned: tests whether a
#                  worker on the core that takes the NIC's interrupts is
#                  the 1-worker rr gap.
#   quinn-copy     quinn writing from a slice (write_all), the copy
#                  giong-quic makes.
# Workloads (WORKLOADS): rr (8 conns x 16 streams, 100 B), bulk (4 conns x
# 1 stream, 16 MiB), many (256 conns x 4 streams, 1 KiB: spreads over
# workers).
# srv_cpu_pct is the server's CPU over the window (100 = one core).
# Results: bench/results/quic/<label>/ (raw.csv, summary.txt, env.txt).
set -euo pipefail
cd "$(dirname "$0")"

ROUNDS=${1:-5}
SECS=${2:-10}
WARMUP=${WARMUP:-3}
: "${SERVER:?set SERVER=user@host}"
: "${SERVER_ADDR:?set SERVER_ADDR=ip:port of the server on the NIC}"
REPO=${REPO:-$(cd .. && pwd)}
SERVER_CPUS=${SERVER_CPUS:-0-7}
CLIENT_CPUS=${CLIENT_CPUS:-0-15}
CLIENT_THREADS=${CLIENT_THREADS:-16}
WORKERS=${WORKERS:-1 4 8}
SERVERS=${SERVERS:-quinn proto-tokio giong giong-seg44 giong-plain giong-epoll}
WORKLOADS=${WORKLOADS:-rr bulk many}
LABEL=${LABEL:-bare-metal-$(date -u +%Y%m%d)}
OUT=results/quic/$LABEL
PORT=${SERVER_ADDR##*:}
mkdir -p "$OUT"
CSV="$OUT/raw.csv"

(cd quic && cargo build --release -q)
ssh "$SERVER" "cd $REPO/bench/quic && cargo build --release -q"
BIN=quic/target/release

server_cmd() { # server workers
  local w=$2 b="$REPO/bench/quic/target/release"
  case $1 in
    quinn) echo "$b/server-quinn --threads $w" ;;
    proto-tokio) echo "$b/server-proto-tokio" ;;
    giong) echo "$b/server-proto-giong --offload 1 --workers $w" ;;
    giong-seg44) echo "$b/server-proto-giong --offload 1 --segments 44 --workers $w" ;;
    giong-plain) echo "$b/server-proto-giong --offload 0" ;;
    giong-epoll) echo "$b/server-proto-giong --offload 1 --epoll 1 --workers $w" ;;
    giong-quic) echo "$b/server-giong-quic --workers $w" ;;
    giong-quic-nopin) echo "$b/server-giong-quic --workers $w --pin 0" ;;
    quinn-copy) echo "$b/server-quinn --copy 1 --threads $w" ;;
  esac
}
single_only() { [[ $1 == proto-tokio || $1 == giong-plain ]]; }
workload_args() {
  case $1 in
    rr) echo "--conns 8 --streams 16 --size 100" ;;
    bulk) echo "--conns 4 --streams 1 --size 16777216" ;;
    many) echo "--conns 256 --streams 4 --size 1024" ;;
  esac
}

{
  echo "# $(date -u +%FT%TZ) client: $(uname -r), $(nproc) cpus,$(grep -m1 'model name' /proc/cpuinfo | cut -d: -f2-)"
  echo "# server: $(ssh "$SERVER" 'uname -r; nproc; grep -m1 "model name" /proc/cpuinfo | cut -d: -f2-' | paste -sd' ')"
  echo "# rounds=$ROUNDS secs=$SECS warmup=$WARMUP server_cpus=$SERVER_CPUS client_cpus=$CLIENT_CPUS threads=$CLIENT_THREADS"
  echo "# server NIC: $(ssh "$SERVER" "ip -br addr | grep -F ${SERVER_ADDR%:*}" || true)"
} > "$OUT/env.txt"
[ -f "$CSV" ] || echo "round,workload,server,workers,req_s,gbit_s,p50_us,p99_us,srv_cpu_pct,srv_us_per_req,srv_ns_per_kib,cli_cpu_pct,errors" > "$CSV"

for round in $(seq 1 "$ROUNDS"); do
  for wl in $WORKLOADS; do
    for s in $SERVERS; do
      for w in $WORKERS; do
        if single_only "$s" && [ "$w" != 1 ]; then continue; fi
        pid=$(ssh "$SERVER" "taskset -c $SERVER_CPUS $(server_cmd "$s" "$w") --addr 0.0.0.0:$PORT >/tmp/quic-server.log 2>&1 & echo \$!")
        sleep 1
        # shellcheck disable=SC2046
        line=$(timeout $((SECS + WARMUP + 60)) taskset -c "$CLIENT_CPUS" "$BIN/client" \
          --addr "$SERVER_ADDR" --server-ssh "$SERVER" --server-pid "$pid" \
          --threads "$CLIENT_THREADS" --warmup "$WARMUP" --secs "$SECS" $(workload_args "$wl") || echo "failed=1")
        ssh "$SERVER" "kill $pid; sleep 0.5; kill -9 $pid 2>/dev/null; true"
        echo "round $round $wl $s x$w: $line" >&2
        vals=$(echo "$line" | tr ' ' '\n' | cut -d= -f2 | paste -sd,)
        echo "$round,$wl,$s,$w,$vals" >> "$CSV"
        sleep 1
      done
    done
  done
done

python3 - "$CSV" > "$OUT/summary.txt" <<'PY'
import csv, statistics, sys
rows = list(csv.DictReader(l for l in open(sys.argv[1]) if not l.startswith('#')))
keys = ["req_s", "gbit_s", "p99_us", "srv_cpu_pct", "srv_us_per_req", "srv_ns_per_kib"]
print("median of rounds (min-max in brackets)")
for w in dict.fromkeys(r["workload"] for r in rows):
    print(f"\n{w}")
    print(f"{'server':<18}" + "".join(f"{k:>26}" for k in keys))
    for s in dict.fromkeys((r["server"], r["workers"]) for r in rows if r["workload"] == w):
        rs = [r for r in rows if r["workload"] == w and (r["server"], r["workers"]) == s and r.get("req_s")]
        cells = []
        for k in keys:
            v = [float(r[k]) for r in rs if r[k] not in ("", None)]
            cells.append(f"{statistics.median(v):.4g} [{min(v):.3g}-{max(v):.3g}]" if v else "-")
        print(f"{s[0] + ' x' + s[1]:<18}" + "".join(f"{c:>26}" for c in cells))
PY
cat "$OUT/summary.txt"
