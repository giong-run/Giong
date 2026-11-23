#!/usr/bin/env bash
# QUIC decision benchmark (plan for 1.1-1.4, section 4): what the I/O layer
# alone buys when quinn-proto runs on giong instead of Tokio.
#
#   bench/quic.sh [rounds] [seconds-per-measurement]
#
# Method
#   * Servers, each on one core (SERVER_CPU), all with the same TLS 1.3
#     certificate, cipher choice and transport settings (bench/quic/src/lib.rs):
#       quinn         quinn 0.11 on a current-thread Tokio runtime (quinn-udp:
#                     GSO and GRO), task per connection and per stream.
#       proto-tokio   quinn-proto driven by bench/quic/src/engine.rs on a
#                     current-thread Tokio runtime, quinn-udp for the socket.
#       giong-plain   the same engine on one giong worker, released UdpSocket
#                     API only (multishot recv_from, one send_to per datagram).
#       giong         the same engine on giong with the unstable-udp-offload
#                     prototype: GRO receive, GSO send of up to 10 datagrams,
#                     ECN.
#       giong-epoll   giong with the offloads on the epoll driver.
#     proto-tokio against giong is the I/O layer alone; quinn against
#     proto-tokio is quinn's own architecture.
#   * Client: quinn on a multi-threaded Tokio runtime on CLIENT_CPUS, one
#     endpoint per thread. Each stream: 8-byte request (size), the server
#     answers with that many bytes.
#       rr     8 connections x 16 streams in flight, 100-byte answers.
#       bulk   4 connections x 1 stream, 16 MiB answers.
#   * WARMUP s of warm-up, then SECS measured. srv_cpu_pct is the server's
#     user+system CPU over the window (100 = one core); srv_us_per_req and
#     srv_ns_per_kib divide it by the work done. On a 4-vCPU VM the client
#     can be the limit: when srv_cpu_pct is well under 100 the server had
#     room to spare and req_s or gbit_s says more about the client than the
#     server; the CPU-per-work columns stay comparable.
#
# SERVERS and WORKLOADS pick a subset. Results: bench/results/quic/<host>/.
# Requires: cargo.
set -euo pipefail
cd "$(dirname "$0")"

ROUNDS=${1:-5}
SECS=${2:-5}
WARMUP=${WARMUP:-2}
SERVER_CPU=${SERVER_CPU:-0}
CLIENT_CPUS=${CLIENT_CPUS:-1-3}
CLIENT_THREADS=${CLIENT_THREADS:-3}
SERVERS=${SERVERS:-quinn proto-tokio giong-plain giong giong-epoll}
WORKLOADS=${WORKLOADS:-rr bulk}
OUT=${OUT:-results/quic/$(hostname -s)}
PORT=${PORT:-4433}
mkdir -p "$OUT"
CSV="$OUT/raw.csv"

(cd quic && cargo build --release -q)
BIN=quic/target/release

server_cmd() {
  case $1 in
    quinn) echo "$BIN/server-quinn" ;;
    proto-tokio) echo "$BIN/server-proto-tokio" ;;
    giong-plain) echo "$BIN/server-proto-giong --offload 0" ;;
    giong) echo "$BIN/server-proto-giong --offload 1" ;;
    giong-epoll) echo "$BIN/server-proto-giong --offload 1 --epoll 1" ;;
  esac
}
workload_args() {
  case $1 in
    rr) echo "--conns 8 --streams 16 --size 100" ;;
    bulk) echo "--conns 4 --streams 1 --size 16777216" ;;
  esac
}

{
  echo "# $(date -u +%FT%TZ) $(uname -r) $(nproc) cpus; $(grep -m1 'model name' /proc/cpuinfo | cut -d: -f2-)"
  echo "# rustc $(rustc --version | cut -d' ' -f2); rounds=$ROUNDS secs=$SECS warmup=$WARMUP server_cpu=$SERVER_CPU client_cpus=$CLIENT_CPUS"
} > "$OUT/env.txt"
[ -f "$CSV" ] || echo "round,workload,server,req_s,gbit_s,p50_us,p99_us,srv_cpu_pct,srv_us_per_req,srv_ns_per_kib,cli_cpu_pct,errors" > "$CSV"

for round in $(seq 1 "$ROUNDS"); do
  for w in $WORKLOADS; do
    for s in $SERVERS; do
      # shellcheck disable=SC2046
      taskset -c "$SERVER_CPU" $(server_cmd "$s") --addr "127.0.0.1:$PORT" 2>"$OUT/server-$s.log" &
      pid=$!
      sleep 0.5
      # shellcheck disable=SC2046
      line=$(timeout $((SECS + WARMUP + 30)) taskset -c "$CLIENT_CPUS" "$BIN/client" \
        --addr "127.0.0.1:$PORT" --server-pid "$pid" --threads "$CLIENT_THREADS" \
        --warmup "$WARMUP" --secs "$SECS" $(workload_args "$w") || echo "failed=1")
      kill "$pid" 2>/dev/null || true
      wait "$pid" 2>/dev/null || true
      echo "round $round $w $s: $line" >&2
      vals=$(echo "$line" | tr ' ' '\n' | cut -d= -f2 | paste -sd,)
      echo "$round,$w,$s,$vals" >> "$CSV"
      sleep 0.5
    done
  done
done

python3 - "$CSV" > "$OUT/summary.txt" <<'PY'
import csv, statistics, sys
rows = list(csv.DictReader(l for l in open(sys.argv[1]) if not l.startswith('#')))
keys = ["req_s", "gbit_s", "p50_us", "p99_us", "srv_cpu_pct", "srv_us_per_req", "srv_ns_per_kib"]
print("median of rounds (min-max in brackets)")
for w in dict.fromkeys(r["workload"] for r in rows):
    print(f"\n{w}")
    print(f"{'server':<13}" + "".join(f"{k:>26}" for k in keys))
    for s in dict.fromkeys(r["server"] for r in rows if r["workload"] == w):
        rs = [r for r in rows if r["workload"] == w and r["server"] == s and r.get("req_s")]
        cells = []
        for k in keys:
            v = [float(r[k]) for r in rs if r[k] not in ("", None)]
            cells.append(f"{statistics.median(v):.4g} [{min(v):.3g}-{max(v):.3g}]" if v else "-")
        print(f"{s:<13}" + "".join(f"{c:>26}" for c in cells))
PY
cat "$OUT/summary.txt"
