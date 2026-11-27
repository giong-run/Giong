#!/bin/bash
# Throughput and latency across worker counts, over the private network (issue #6, again on
# 1.5.x): the server on this machine (br-1), bench/lab/loadgen on br-2.
#
#   OUT=/root/r15/out/rate bash rate.sh [rounds] [seconds]
#
#   SERVERS   label=binary:port (default: giong-http from main on 3009, tokio-raw on 3005)
#   WORKERS   default "1 2 4 8": the first N of SERVER_CPUS (0-7, one per physical core)
#   PATHS     default "/json /users"
#
# For every server, worker count and path, each round measures:
#   * closed loop (rate 0): 64 connections per worker, each sending its next request when the
#     last answer is in: the highest throughput;
#   * open loop at every rate of RATES up to 1.1 times that round's closed-loop throughput:
#     requests fall due at the rate, and each latency counts from when its request fell due
#     (no coordinated omission). Rates are absolute, so worker counts share them and p99 at
#     the same rate can be compared.
# Recorded per measurement: offered and answered requests per second, p50, p99, p99.9 and max,
# errors, the server's CPU time per request and how busy its cores were, how busy br-2's
# CPUs were, and the bytes per second on the server's interface (both directions, Gbit/s),
# so that each row says what limited it: the server, the load generator or the link.
# Servers take turns in a rotating order within each round.
. "$(dirname "$0")/env.sh"
ROUNDS=${1:-3}
SECS=${2:-10}
OUT=${OUT:-$R15/out/rate}
read -r -a SRV <<<"${SERVERS:-giong-http=$B/bins/main-giong-http:3009 tokio-raw=$B/bins/main-tokio-raw:3005}"
read -r -a WORKERS <<<"${WORKERS:-1 2 4 8}"
read -r -a PATHS_ <<<"${PATHS:-/json /users}"
RATES=${RATES:-25000 50000 100000 200000 300000 400000 500000 700000 1000000 1400000 2000000 2600000 3200000}
SERVER_CPUS=${SERVER_CPUS:-0-7}
LOAD_THREADS=${LOAD_THREADS:-16}
LOADGEN=${LOADGEN:-$B/bins/lab-loadgen}
[ -n "$S" ] && [ -n "$L" ] || { echo "no /root/r15/hosts (S=..., L=...)" >&2; exit 2; }
mkdir -p "$OUT/raw"
CSV=$OUT/raw.csv
expand() { local o=() p; IFS=, read -r -a ps <<<"$1"; for p in "${ps[@]}"; do if [[ $p == *-* ]]; then o+=($(seq "${p%-*}" "${p#*-}")); else o+=("$p"); fi; done; echo "${o[@]}"; }
read -r -a CPUS <<<"$(expand "$SERVER_CPUS")"
IF=$(iface_of "$S")
{
  echo "# rate.sh $(date -u +%FT%TZ): server $(hostname) $(uname -r) on CPUs $SERVER_CPUS ($IF, $(ethtool "$IF" 2>/dev/null | awk '/Speed/{print $2}')), loadgen on $L with $LOAD_THREADS threads, $SECS s per measurement after 2 s of warm-up, 64 connections per worker"
  for s in "${SRV[@]}"; do echo "# ${s%%=*} = ${s#*=}"; done
  echo "# rates: $RATES"
} >"$OUT/conditions.txt"
[ -f "$CSV" ] || echo "server,workers,path,round,rate,conns,rps,goodput_rps,p50_us,p99_us,p999_us,max_us,errors,server_busy,us_per_req,load_busy,gbit_s" >"$CSV"
to_l "$LOADGEN" || { echo "cannot copy the loadgen to $L" >&2; exit 2; }

one() { # label n path rate pid port -> appends a row, prints the rps
  local label=$1 n=$2 path=$3 rate=$4 pid=$5 port=$6 conns=$((64 * $2)) cpus=("${CPUS[@]:0:$2}")
  local t0 b0 bt0 lb0 lt0 x0 t1 b1 bt1 lb1 lt1 x1 js f
  t0=$(ticks "$pid"); read -r b0 bt0 <<<"$(cpu_busy "${cpus[@]}")"; x0=$(nic_bytes "$IF")
  read -r lb0 lt0 <<<"$(on_l "$(declare -f cpu_busy); cpu_busy all")"
  js=$(on_l "/root/lab-loadgen --addr $S:$port --conns $conns --threads $LOAD_THREADS --duration $SECS --warmup 2 --rate $rate --mix $path" 2>>"$OUT/loadgen.err")
  t1=$(ticks "$pid"); read -r b1 bt1 <<<"$(cpu_busy "${cpus[@]}")"; x1=$(nic_bytes "$IF")
  read -r lb1 lt1 <<<"$(on_l "$(declare -f cpu_busy); cpu_busy all")"
  f=$OUT/raw/$label-w$n-${path//\//}-r$r-$rate.json
  echo "$js" >"$f"
  # CPU and bytes cover warm-up plus measurement; per request uses every answer in that time.
  python3 - "$f" "$label" "$n" "$path" "$r" "$rate" "$conns" $((t1 - t0)) "$HZ" $((b1 - b0)) $((bt1 - bt0)) $((lb1 - lb0)) $((lt1 - lt0)) $((x1 - x0)) "$SECS" >>"$CSV" <<'PY'
import json, sys
f, label, n, path, r, rate, conns, ticks, hz, b, bt, lb, lt, x, secs = sys.argv[1:]
try:
    j = json.load(open(f))
except Exception:
    j = {"rps": 0, "goodput_rps": 0, "latency": {}, "errors": -1, "requests": 0}
lat = j.get("latency", {})
secs = float(secs)
wall = secs + 2.0
answered = j.get("goodput_rps", 0) * secs
us = int(ticks) * 1e6 / int(hz) / answered if answered else 0
print(f"{label},{n},{path},{r},{rate},{conns},{j.get('rps', 0)},{j.get('goodput_rps', 0)},"
      f"{lat.get('p50_us', 0)},{lat.get('p99_us', 0)},{lat.get('p999_us', 0)},{lat.get('max_us', 0)},"
      f"{j.get('errors', 0) + j.get('connect_errors', 0) + j.get('non_2xx', 0)},"
      f"{int(b) / int(bt) if int(bt) else 0:.3f},{us * secs / wall:.3f},{int(lb) / int(lt) if int(lt) else 0:.3f},"
      f"{int(x) * 8 / wall / 1e9:.2f}")
PY
  tail -n1 "$CSV" | cut -d, -f7
}

labels=(); for s in "${SRV[@]}"; do labels+=("${s%%=*}"); done
for r in $(seq 1 "$ROUNDS"); do
  for n in "${WORKERS[@]}"; do
    for path in "${PATHS_[@]}"; do
      k=$(( (r - 1) % ${#labels[@]} ))
      for label in "${labels[@]:$k}" "${labels[@]:0:$k}"; do
        spec=""; for s in "${SRV[@]}"; do [ "${s%%=*}" = "$label" ] && spec=${s#*=}; done
        bin=${spec%:*} port=${spec##*:}
        BENCH_HOST=$S taskset -c "$(IFS=,; echo "${CPUS[*]:0:$n}")" "$bin" >/dev/null 2>>"$OUT/server.log" &
        pid=$!
        sleep 1.5
        max=$(one "$label" "$n" "$path" 0 "$pid" "$port")
        for rate in $RATES; do
          awk -v r="$rate" -v m="${max:-0}" 'BEGIN{exit !(r <= 1.1 * m)}' || break
          one "$label" "$n" "$path" "$rate" "$pid" "$port" >/dev/null
        done
        kill "$pid" 2>/dev/null; wait "$pid" 2>/dev/null
        sleep 0.5
      done
    done
    log "rate round $r/$ROUNDS, $n worker(s) done"
  done
done
python3 "$(dirname "$0")/summarize.py" rate "$CSV" | tee "$OUT/summary.txt"
