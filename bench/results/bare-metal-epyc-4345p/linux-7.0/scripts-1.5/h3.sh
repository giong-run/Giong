#!/bin/bash
# HTTP/3 requests over the private network: giong-http's Server::serve_h3 on giong-quic
# (h3-server-giong) against the h3 crate on quinn and Tokio (h3-server-tokio), both from
# bench/h3, on this machine (br-1); bench/h3's closed-loop client (h3 + quinn + Tokio, not
# giong's own client) on br-2.
#
#   OUT=/root/r15/out/h3 bash h3.sh [rounds] [seconds]
#
#   WORKERS   default "1 4" (first N of SERVER_CPUS 0-7). giong runs an endpoint on every
#             worker, all on the same port; quinn has one endpoint, on Tokio's
#             current-thread runtime for one worker and the multi-threaded one otherwise.
#   CASES     name:path:connections per worker:streams per connection
#             (default json:/json:16:8 users:/users:16:8 json-mux:/json:2:64)
#
# Recorded per measurement: requests per second, p50, p99 (from sending a request to its
# last byte), errors, the server's CPU time per request and how busy its cores were, how
# busy br-2's CPUs were. UDP counters (netstat -su) of both machines go to udp.txt before
# and after each measurement, for receive-buffer drops. Network sysctls stay at Ubuntu's
# defaults, as everywhere in this run.
. "$(dirname "$0")/env.sh"
ROUNDS=${1:-5}
SECS=${2:-10}
OUT=${OUT:-$R15/out/h3}
read -r -a SERVERS <<<"${SERVERS:-giong=$B/bins/h3-server-giong quinn=$B/bins/h3-server-tokio}"
read -r -a WORKERS <<<"${WORKERS:-1 4}"
read -r -a CASES <<<"${CASES:-json:/json:16:8 users:/users:16:8 json-mux:/json:2:64}"
SERVER_CPUS=${SERVER_CPUS:-0-7}
LOAD_THREADS=${LOAD_THREADS:-16}
[ -n "$S" ] && [ -n "$L" ] || { echo "no /root/r15/hosts (S=..., L=...)" >&2; exit 2; }
mkdir -p "$OUT/raw"
CSV=$OUT/raw.csv
expand() { local o=() p; IFS=, read -r -a ps <<<"$1"; for p in "${ps[@]}"; do if [[ $p == *-* ]]; then o+=($(seq "${p%-*}" "${p#*-}")); else o+=("$p"); fi; done; echo "${o[@]}"; }
read -r -a CPUS <<<"$(expand "$SERVER_CPUS")"
TMP=$(mktemp -d); trap 'rm -rf "$TMP"' EXIT
# rustls refuses a CA certificate as the server's own, so this one says CA:FALSE.
openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:prime256v1 -nodes -keyout "$TMP/key.pem" -out "$TMP/cert.pem" \
  -days 2 -subj "/CN=localhost" -addext "subjectAltName=DNS:localhost,IP:$S" -addext "basicConstraints=critical,CA:FALSE" 2>/dev/null
to_l "$B/bins/h3-client" "$TMP/cert.pem" || exit 2
{
  echo "# h3.sh $(date -u +%FT%TZ): server $(hostname) $(uname -r) on CPUs $SERVER_CPUS, client on $L with $LOAD_THREADS threads, $SECS s per measurement after 2 s of warm-up"
  echo "# bench/h3 from $(git -C "$B/run" rev-parse --short HEAD 2>/dev/null) ($(git -C "$B/run" log -1 --format=%s 2>/dev/null | cut -c1-60))"
  echo "# rmem_max $(sysctl -n net.core.rmem_max), wmem_max $(sysctl -n net.core.wmem_max); br-2: $(on_l 'sysctl -n net.core.rmem_max net.core.wmem_max' | paste -sd' ')"
  for s in "${SERVERS[@]}"; do echo "# ${s%%=*} = ${s#*=}"; done
} >"$OUT/conditions.txt"
[ -f "$CSV" ] || echo "server,workers,case,round,order,rps,p50_us,p99_us,p999_us,max_us,errors,server_busy,us_per_req,load_busy" >"$CSV"
udp() { echo "== $1 br-1"; nstat -asz 'Udp*' | grep -E 'Errors'; echo "== $1 br-2"; on_l "nstat -asz 'Udp*' | grep -E 'Errors'"; }

labels=(); for s in "${SERVERS[@]}"; do labels+=("${s%%=*}"); done
for r in $(seq 1 "$ROUNDS"); do
  for n in "${WORKERS[@]}"; do
    for c in "${CASES[@]}"; do
      IFS=: read -r name path cpw streams <<<"$c"
      k=$(( (r - 1) % ${#labels[@]} )); pos=0
      for label in "${labels[@]:$k}" "${labels[@]:0:$k}"; do
        pos=$((pos + 1))
        bin=""; for s in "${SERVERS[@]}"; do [ "${s%%=*}" = "$label" ] && bin=${s#*=}; done
        cpus=("${CPUS[@]:0:$n}")
        BENCH_HOST=$S TLS_CERT=$TMP/cert.pem TLS_KEY=$TMP/key.pem taskset -c "$(IFS=,; echo "${cpus[*]}")" "$bin" >/dev/null 2>>"$OUT/server.log" &
        pid=$!
        sleep 1.5
        udp "before $label w$n $name r$r" >>"$OUT/udp.txt" 2>&1
        t0=$(ticks "$pid"); read -r b0 bt0 <<<"$(cpu_busy "${cpus[@]}")"; read -r lb0 lt0 <<<"$(on_l "$(declare -f cpu_busy); cpu_busy all")"
        f=$OUT/raw/$label-w$n-$name-r$r.json
        on_l "/root/h3-client --addr $S:3012 --ca /root/cert.pem --conns $((cpw * n)) --streams $streams --threads $LOAD_THREADS --duration $SECS --warmup 2 --path $path" >"$f" 2>>"$OUT/client.err"
        t1=$(ticks "$pid"); read -r b1 bt1 <<<"$(cpu_busy "${cpus[@]}")"; read -r lb1 lt1 <<<"$(on_l "$(declare -f cpu_busy); cpu_busy all")"
        udp "after $label w$n $name r$r" >>"$OUT/udp.txt" 2>&1
        kill "$pid" 2>/dev/null; wait "$pid" 2>/dev/null
        python3 - "$f" "$label,$n,$name,$r,$pos" $((t1 - t0)) "$HZ" $((b1 - b0)) $((bt1 - bt0)) $((lb1 - lb0)) $((lt1 - lt0)) "$SECS" >>"$CSV" <<'PY'
import json, sys
f, head, ticks, hz, b, bt, lb, lt, secs = sys.argv[1:]
try:
    j = json.load(open(f))
except Exception:
    j = {"rps": 0, "errors": -1}
secs = float(secs)
n = j.get("requests", 0)
# CPU covers the warm-up too: per request scales it to the measured part.
us = int(ticks) * 1e6 / int(hz) / n * secs / (secs + 2) if n else 0
print(f"{head},{j.get('rps', 0)},{j.get('p50_us', 0)},{j.get('p99_us', 0)},{j.get('p999_us', 0)},{j.get('max_us', 0)},"
      f"{j.get('errors', 0) + j.get('connect_errors', 0)},{int(b) / int(bt) if int(bt) else 0:.3f},{us:.3f},"
      f"{int(lb) / int(lt) if int(lt) else 0:.3f}")
PY
        sleep 0.5
      done
    done
    log "h3 round $r/$ROUNDS, $n worker(s) done"
  done
done
python3 "$(dirname "$0")/summarize.py" h3 "$CSV" | tee "$OUT/summary.txt"
