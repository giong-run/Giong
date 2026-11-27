#!/bin/bash
# The whole-service test asked for in #428/#429 (an outside PR, measured only): giong-web
# with gzip on (MIDDLEWARE=1: request ids, Compression::new() at level 1 for bodies of 1 KiB
# and more, a timeout), built from PR #429 with its default miniz_oxide path and with
# giong-web/fast-gzip (libdeflater for level-1 bodies up to 16 KiB). The server runs here
# (br-1), bench/lab/loadgen on br-2 asks for `Accept-Encoding: gzip`.
#
#   OUT=/root/r15/out/gzip bash gzip.sh [rounds] [seconds]
#
#   PATHS     /users (8.3 KB of JSON, sent as about 1.1 KB of gzip) and /json (27 bytes,
#             under min_size: never compressed, so both builds should match there)
#   WORKERS   default "1 4" (first N of SERVER_CPUS 0-7)
#
# Each round, for each build in a rotating order:
#   * closed loop, 64 connections per worker: requests per second, server CPU per request;
#   * open loop at 50% and 80% of the miniz build's closed-loop rate of that round (the same
#     rates for both builds): p50, p99, p99.9, max;
#   * the server's resident memory (VmRSS) after the measurements and its peak (VmHWM),
#     against the same process before any load: what the per-worker compressor state keeps.
. "$(dirname "$0")/env.sh"
ROUNDS=${1:-6}
SECS=${2:-10}
OUT=${OUT:-$R15/out/gzip}
read -r -a BUILDS <<<"${BUILDS:-miniz=$B/bins/pr429-web-miniz fast=$B/bins/pr429-web-fast}"
read -r -a WORKERS <<<"${WORKERS:-1 4}"
read -r -a PATHS_ <<<"${PATHS:-/users /json}"
SERVER_CPUS=${SERVER_CPUS:-0-7}
LOAD_THREADS=${LOAD_THREADS:-16}
PORT=3011
[ -n "$S" ] && [ -n "$L" ] || { echo "no /root/r15/hosts (S=..., L=...)" >&2; exit 2; }
mkdir -p "$OUT/raw"
CSV=$OUT/raw.csv
expand() { local o=() p; IFS=, read -r -a ps <<<"$1"; for p in "${ps[@]}"; do if [[ $p == *-* ]]; then o+=($(seq "${p%-*}" "${p#*-}")); else o+=("$p"); fi; done; echo "${o[@]}"; }
read -r -a CPUS <<<"$(expand "$SERVER_CPUS")"
to_l "$B/bins/lab-loadgen" || exit 2
start() { # bin n -> pid
  MIDDLEWARE=1 BENCH_HOST=$S taskset -c "$(IFS=,; echo "${CPUS[*]:0:$2}")" "$1" >/dev/null 2>>"$OUT/server.log" &
  echo $!
}
kib() { awk -v k="$2" '$1 == k":" {print $2}' /proc/"$1"/status; }
{
  echo "# gzip.sh $(date -u +%FT%TZ): server $(hostname) $(uname -r) on CPUs $SERVER_CPUS, loadgen on $L with $LOAD_THREADS threads and 'Accept-Encoding: gzip', $SECS s per measurement after 2 s of warm-up"
  echo "# PR #429 at $(git -C "$B/pr429" rev-parse --short HEAD 2>/dev/null); bench/giong-web there binds BENCH_HOST:3011 (build.sh)"
  for it in "${BUILDS[@]}"; do
    bin=${it#*=}; pid=$(start "$bin" 1); sleep 1.5
    for p in "${PATHS_[@]}"; do
      echo "# ${it%%=*} $p: $(on_l "curl -s -o /dev/null -w '%{http_code} %{size_download} bytes' -H 'Accept-Encoding: gzip' http://$S:$PORT$p"), headers: $(on_l "curl -s -D - -o /dev/null -H 'Accept-Encoding: gzip' http://$S:$PORT$p" | grep -i -E 'content-(encoding|length)' | tr -d '\r' | paste -sd' ')"
    done
    kill "$pid"; wait "$pid" 2>/dev/null; sleep 0.5
  done
} >"$OUT/conditions.txt"
[ -f "$CSV" ] || echo "build,workers,path,round,mode,rate,rps,p50_us,p99_us,p999_us,max_us,errors,server_busy,us_per_req,load_busy,rss0_kib,rss_kib,hwm_kib" >"$CSV"

measure() { # label pid n path mode rate
  local label=$1 pid=$2 n=$3 path=$4 mode=$5 rate=$6 cpus=("${CPUS[@]:0:$3}")
  local t0 b0 bt0 lb0 lt0 t1 b1 bt1 lb1 lt1 f
  t0=$(ticks "$pid"); read -r b0 bt0 <<<"$(cpu_busy "${cpus[@]}")"
  read -r lb0 lt0 <<<"$(on_l "$(declare -f cpu_busy); cpu_busy all")"
  f=$OUT/raw/$label-w$n-${path//\//}-r$r-$mode.json
  on_l "/root/lab-loadgen --addr $S:$PORT --conns $((64 * n)) --threads $LOAD_THREADS --duration $SECS --warmup 2 --rate $rate --mix $path --header 'Accept-Encoding: gzip'" >"$f" 2>>"$OUT/loadgen.err"
  t1=$(ticks "$pid"); read -r b1 bt1 <<<"$(cpu_busy "${cpus[@]}")"
  read -r lb1 lt1 <<<"$(on_l "$(declare -f cpu_busy); cpu_busy all")"
  python3 - "$f" "$label,$n,$path,$r,$mode,$rate" $((t1 - t0)) "$HZ" $((b1 - b0)) $((bt1 - bt0)) $((lb1 - lb0)) $((lt1 - lt0)) "$SECS" "$RSS0" "$(kib "$pid" VmRSS)" "$(kib "$pid" VmHWM)" >>"$CSV" <<'PY'
import json, sys
f, head, ticks, hz, b, bt, lb, lt, secs, rss0, rss, hwm = sys.argv[1:]
try:
    j = json.load(open(f))
except Exception:
    j = {"rps": 0, "goodput_rps": 0, "latency": {}, "errors": -1}
lat = j.get("latency", {})
secs = float(secs)
answered = j.get("goodput_rps", 0) * secs
us = int(ticks) * 1e6 / int(hz) / answered * secs / (secs + 2) if answered else 0
print(f"{head},{j.get('rps', 0)},{lat.get('p50_us', 0)},{lat.get('p99_us', 0)},{lat.get('p999_us', 0)},{lat.get('max_us', 0)},"
      f"{j.get('errors', 0) + j.get('connect_errors', 0) + j.get('non_2xx', 0)},"
      f"{int(b) / int(bt) if int(bt) else 0:.3f},{us:.3f},{int(lb) / int(lt) if int(lt) else 0:.3f},{rss0},{rss},{hwm}")
PY
  tail -n1 "$CSV" | cut -d, -f7
}

labels=(); for it in "${BUILDS[@]}"; do labels+=("${it%%=*}"); done
for r in $(seq 1 "$ROUNDS"); do
  for n in "${WORKERS[@]}"; do
    for path in "${PATHS_[@]}"; do
      k=$(( (r - 1) % ${#labels[@]} ))
      declare -A max=()
      # Closed loop first for every build, so that the open-loop rates come from this round.
      for pass in closed open; do
        for label in "${labels[@]:$k}" "${labels[@]:0:$k}"; do
          bin=""; for it in "${BUILDS[@]}"; do [ "${it%%=*}" = "$label" ] && bin=${it#*=}; done
          pid=$(start "$bin" "$n"); sleep 1.5
          RSS0=$(kib "$pid" VmRSS)
          if [ $pass = closed ]; then
            max[$label]=$(measure "$label" "$pid" "$n" "$path" closed 0)
          else
            base=${max[${labels[0]}]:-0}
            for pct in 50 80; do
              rate=$(awk -v m="$base" -v p=$pct 'BEGIN{printf "%d", m * p / 100}')
              [ "$rate" -gt 0 ] && measure "$label" "$pid" "$n" "$path" "open$pct" "$rate" >/dev/null
            done
          fi
          kill "$pid" 2>/dev/null; wait "$pid" 2>/dev/null; sleep 0.5
        done
      done
      unset max
    done
    log "gzip round $r/$ROUNDS, $n worker(s) done"
  done
done
python3 "$(dirname "$0")/summarize.py" gzip "$CSV" | tee "$OUT/summary.txt"
