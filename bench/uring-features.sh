#!/usr/bin/env bash
# Which optional io_uring features pay off? giong-http with each feature
# turned off (GIONG_IO_URING_OFF) against all of them on, interleaved.
#
#   bench/uring-features.sh [rounds] [seconds-per-measurement]
#
# Same machine (the default): the server gets SERVER_CPUS, wrk LOAD_CPUS, as
# in bench/workers.sh. Loopback always copies, so zero-copy send can only
# show what it buys over a real network: run the server here, bound to
# BENCH_HOST, and wrk on LOAD_HOST over ssh.
#
#   BENCH_HOST=10.0.0.2 LOAD_HOST=bench@10.0.0.3 bench/uring-features.sh 5 10
#
# PATHS are the requests (default: /json, bodies of 4 KiB to 1 MiB from
# /static?size=N, and one connection fetching 64 MiB streamed in 64 KiB pieces
# from /stream, which shows what waiting for each piece costs); "@N" after a
# path runs it with N connections instead of CONNS. VARIANTS are
# "name:VAR=value,VAR=value" (default: every feature on, then each one off,
# then all off; GIONG_WORKERS=<n> fits there too). In each round every variant
# runs once per path, starting from a different one each round. Recorded per
# run: requests per second, p99, the server's CPU time per request (user and
# kernel, from /proc) and the bytes per second wrk received. The summary is
# the median per path and variant and its change against "all-on".
set -euo pipefail
cd "$(dirname "$0")"

ROUNDS=${1:-5}
SECS=${2:-8}
CONNS=${CONNS:-256}
BENCH_HOST=${BENCH_HOST:-127.0.0.1}
LOAD_HOST=${LOAD_HOST:-}
OUT=${OUT:-results/uring-features}
read -r -a PATHS <<<"${PATHS:-/json /static?size=4096 /static?size=16384 /static?size=65536 /static?size=262144 /static?size=1048576 /stream?size=67108864&chunk=65536@1}"
read -r -a VARIANTS <<<"${VARIANTS:-all-on: no-zero-copy:GIONG_IO_URING_OFF=zero-copy-send no-registered-ring:GIONG_IO_URING_OFF=registered-ring no-packed-buffers:GIONG_IO_URING_OFF=packed-buffers all-off:GIONG_IO_URING_OFF=all}"

ncpu=$(nproc)
if [ -n "$LOAD_HOST" ]; then
  SERVER_CPUS=${SERVER_CPUS:-0-$((ncpu - 1))}
  LOAD_THREADS=${LOAD_THREADS:-$(ssh "$LOAD_HOST" nproc)}
else
  half=$((ncpu / 2))
  SERVER_CPUS=${SERVER_CPUS:-$half-$((ncpu - 1))}
  LOAD_CPUS=${LOAD_CPUS:-0-$((half - 1))}
  LOAD_THREADS=${LOAD_THREADS:-$half}
fi
TICKS=$(getconf CLK_TCK)
mkdir -p "$OUT"
CSV="$OUT/raw.csv"
ulimit -n "$(ulimit -Hn)"

echo "building..." >&2
(cd giong-http && cargo build --release -q)
SERVER=giong-http/target/release/bench-giong-http

cpu_ticks() { cat /proc/"$1"/task/*/stat 2>/dev/null | awk '{u+=$14; s+=$15} END{print u+0, s+0}'; }
load() { # wrk arguments...
  if [ -n "$LOAD_HOST" ]; then ssh "$LOAD_HOST" wrk "$@"; else taskset -c "$LOAD_CPUS" wrk "$@"; fi
}
# "1.5GB" -> bytes
to_bytes() { awk -v v="$1" 'BEGIN{n=v+0; u=substr(v, length(n "")+1); m=1;
  if (u ~ /^K/) m=1024; else if (u ~ /^M/) m=1048576; else if (u ~ /^G/) m=1073741824; printf "%.0f", n*m}'; }

echo "path,variant,round,rps,p99,user_us,kernel_us,mib_s,features" >"$CSV"
for round in $(seq 1 "$ROUNDS"); do
  for spec in "${PATHS[@]}"; do
    path=${spec%@*}
    conns=$CONNS
    [ "$spec" != "$path" ] && conns=${spec##*@}
    threads=$((conns < LOAD_THREADS ? conns : LOAD_THREADS))
    n=${#VARIANTS[@]}
    for i in $(seq 0 $((n - 1))); do
      v=${VARIANTS[$(((i + round) % n))]}
      name=${v%%:*}
      envs=${v#*:}
      env ${envs//,/ } BENCH_HOST="$BENCH_HOST" GIONG_LOG_DRIVER=1 \
        taskset -c "$SERVER_CPUS" "$SERVER" >/dev/null 2>"$OUT/server.err" &
      pid=$!
      sleep 1.2
      url="http://$BENCH_HOST:3009$path"
      load -t"$threads" -c"$conns" -d2s --timeout 5s "$url" >/dev/null 2>&1
      read -r u0 k0 <<<"$(cpu_ticks "$pid")"
      out=$(load -t"$threads" -c"$conns" -d"${SECS}s" --timeout 5s --latency "$url")
      read -r u1 k1 <<<"$(cpu_ticks "$pid")"
      kill "$pid" 2>/dev/null || true
      wait "$pid" 2>/dev/null || true
      reqs=$(awk '/requests in/{print $1}' <<<"$out")
      rps=$(awk '/Requests\/sec/{print $2}' <<<"$out")
      p99=$(awk '$1=="99%"{print $2}' <<<"$out")
      bytes=$(to_bytes "$(awk '/requests in/{print $5}' <<<"$out" | tr -d ',')")
      read -r uus kus <<<"$(awk -v r="$reqs" -v u=$((u1 - u0)) -v k=$((k1 - k0)) -v t="$TICKS" \
        'BEGIN{printf "%.3f %.3f", u/t/r*1e6, k/t/r*1e6}')"
      mibs=$(awk -v b="$bytes" -v s="$SECS" 'BEGIN{printf "%.1f", b/s/1048576}')
      features=$(grep -o 'using [a-z, -]*' "$OUT/server.err" | head -1 | sed 's/using //; s/, /+/g' || true)
      echo "\"$spec\",$name,$round,$rps,$p99,$uus,$kus,$mibs,${features:-none}" >>"$CSV"
      sleep 0.5
    done
  done
  echo "round $round/$ROUNDS done" >&2
done

python3 - "$CSV" <<'PY' | tee "$OUT/summary.txt"
import csv, statistics, sys
rows = list(csv.DictReader(open(sys.argv[1])))
paths = list(dict.fromkeys(r["path"] for r in rows))
variants = list(dict.fromkeys(r["variant"] for r in rows))
med = lambda p, v, k: statistics.median(float(r[k]) for r in rows if r["path"] == p and r["variant"] == v)
for p in paths:
    print(p)
    print(f"  {'variant':20} {'req/s':>10} {'vs all-on':>9} {'CPU us/req':>11} {'vs all-on':>9} {'MiB/s':>8}")
    base = (med(p, variants[0], "rps"), med(p, variants[0], "user_us") + med(p, variants[0], "kernel_us"))
    for v in variants:
        rps = med(p, v, "rps")
        cpu = med(p, v, "user_us") + med(p, v, "kernel_us")
        print(f"  {v:20} {rps:10.0f} {rps / base[0] - 1:+9.1%} {cpu:11.2f} {cpu / base[1] - 1:+9.1%} {med(p, v, 'mib_s'):8.1f}")
    print()
PY
