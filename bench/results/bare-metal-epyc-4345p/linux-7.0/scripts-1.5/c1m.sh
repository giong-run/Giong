#!/bin/bash
# C1M: a million concurrent connections held by bench/giong-http on br-1 (8 workers, CPUs
# 0-7), opened from br-2 by bench/c1m (c1m-hold), each with one request answered and then
# quiet, the way keep-alive clients sit between requests.
#
#   OUT=/root/r15/out/c1m bash c1m.sh
#
#   BUILDS   label=server binary (default: main from this branch's bench/giong-http, which
#            listens on a range of ports with BENCH_PORTS; PR #487 built with the same
#            main.rs, while that PR is open)
#   MODES    http1 h2c https h2tls (HTTP/1.1, HTTP/2 with prior knowledge, HTTPS with ALPN
#            http/1.1, HTTP/2 over TLS with ALPN h2; TLS 1.3, full handshakes, ECDSA P-256)
#   CONNS    1000000
#
# One machine opens at most about 64,000 connections to one address and port (the local
# port range is set to 1024-65535 on br-2 for the run), so the server listens on 20 ports
# and connection i goes to port i mod 20: 50,000 per port. Every run uses ports of its own
# (3100-3119, 3200-3219, ...), so that sockets left in TIME_WAIT by the run before never
# collide with the next one.
#
# For each run:
#   1. a probe with wrk from br-2 (4 threads, 64 connections, 10 s, keep-alive GET /json)
#      against the fresh server: requests/s, p50, p99;
#   2. c1m-hold opens CONNS connections (16 threads): how long that takes;
#   3. with every connection held: the server's resident memory (RssAnon, VmRSS), the memory
#      both machines lost (MemAvailable), sockets and their kernel memory
#      (/proc/net/sockstat), and the server's CPU over 30 s of quiet;
#   4. the same probe again, with the million connections held.
# Per connection = growth over the run / connections held.
#
# The run changes, and restores at the end: fs.nr_open (2,097,152, so that a process may hold
# more than a million descriptors) on both machines, the local port range on br-2, and
# nf_conntrack_max with its hash size on both (ufw tracks every connection; the default
# table holds 262,144). Everything else stays as it is.
D=$(cd "$(dirname "$0")" && pwd)
. "$D/env.sh"
OUT=${OUT:-$R15/out/c1m}
read -r -a MODES <<<"${MODES:-http1 h2c https h2tls}"
CONNS=${CONNS:-1000000}
NPORTS=${NPORTS:-20}
HOLD_THREADS=${HOLD_THREADS:-16}
builds="main=$B/bins/c1m-main"; [ -x "$B/bins/c1m-pr487" ] && builds="$builds pr487=$B/bins/c1m-pr487"
read -r -a BUILDS <<<"${BUILDS:-$builds}"
[ -n "$S" ] && [ -n "$L" ] || { echo "no /root/r15/hosts (S=..., L=...)" >&2; exit 2; }
mkdir -p "$OUT/raw"
CSV=$OUT/raw.csv
TMP=$(mktemp -d); trap 'rm -rf "$TMP"' EXIT
openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:prime256v1 -nodes -keyout "$TMP/key.pem" -out "$TMP/cert.pem" \
  -days 2 -subj "/CN=localhost" -addext "subjectAltName=DNS:localhost,IP:$S" -addext "basicConstraints=critical,CA:FALSE" 2>/dev/null
to_l "$B/bins/c1m-hold" "$TMP/cert.pem" || exit 2

# --- settings for the run, restored at the end
both() { bash -c "$1"; on_l "$1"; }
CT=/proc/sys/net/netfilter/nf_conntrack_max
HS=/sys/module/nf_conntrack/parameters/hashsize
save="nr_open=$(cat /proc/sys/fs/nr_open) ct=$(cat $CT 2>/dev/null) hs=$(cat $HS 2>/dev/null)"
save_l=$(on_l "echo nr_open=\$(cat /proc/sys/fs/nr_open) ct=\$(cat $CT 2>/dev/null) hs=\$(cat $HS 2>/dev/null) range=\\\"\$(cat /proc/sys/net/ipv4/ip_local_port_range | tr '\t' ' ')\\\"")
echo "# settings before: br-1 $save; br-2 $save_l" >>"$OUT/conditions.txt"
restore() {
  eval "$save"; sysctl -q -w fs.nr_open="$nr_open"
  [ -n "$ct" ] && echo "$ct" >$CT; [ -n "$hs" ] && echo "$hs" >$HS
  on_l "$save_l; sysctl -q -w fs.nr_open=\$nr_open; [ -n \"\$ct\" ] && echo \$ct >$CT; [ -n \"\$hs\" ] && echo \$hs >$HS; sysctl -q -w net.ipv4.ip_local_port_range=\"\$range\""
  log "c1m: settings restored"
}
trap 'restore; rm -rf "$TMP"' EXIT
both "sysctl -q -w fs.nr_open=2097152; [ -w $CT ] && echo 4194304 >$CT; [ -w $HS ] && echo 1048576 >$HS; true"
on_l "sysctl -q -w net.ipv4.ip_local_port_range='1024 65535'"
ulimit -Hn 2000000 && ulimit -n 2000000
{
  echo "# c1m.sh $(date -u +%FT%TZ): server $(hostname) $(uname -r), 8 workers on CPUs 0-7, $NPORTS ports; c1m-hold on $L, $HOLD_THREADS threads; $CONNS connections"
  echo "# during the run: fs.nr_open 2097152, nf_conntrack_max $(cat $CT 2>/dev/null || echo -), br-2 ip_local_port_range '$(on_l 'cat /proc/sys/net/ipv4/ip_local_port_range' | tr '\t' ' ')', ulimit -n $(ulimit -n)"
  for b in "${BUILDS[@]}"; do echo "# ${b%%=*} = ${b#*=}"; done
} >>"$OUT/conditions.txt"
[ -f "$CSV" ] || echo "build,mode,conns,held,errors,open_secs,open_per_s,server_rssanon_kib_per_conn,server_vmrss_mib,server_avail_drop_mib,server_total_kib_per_conn,client_avail_drop_mib,client_total_kib_per_conn,tcp_inuse,tcp_mem_mib,conntrack,idle_cpu_pct,base_rps,base_p50,base_p99,held_rps,held_p50,held_p99,probe_errors" >"$CSV"

kib() { awk -v k="$2" '$1 == k":" {print $2}' /proc/"$1"/status; }
avail() { awk '/^MemAvailable/{print $2}' /proc/meminfo; }
probe() { # url -> "rps p50 p99 errors" (p50/p99 in microseconds)
  local out
  out=$(on_l "$(printf '%q ' wrk -t4 -c64 -d10s --timeout 5s --latency "$1")" 2>&1)
  awk 'function us(v) { if (v ~ /us$/) return v + 0; if (v ~ /ms$/) return v * 1000; if (v ~ /s$/) return v * 1e6; return v }
       /Requests\/sec/ {r = $2} $1 == "50%" {p50 = us($2)} $1 == "99%" {p99 = us($2)}
       /Socket errors/ {gsub(/,/, ""); e += $4 + $6 + $8 + $10} /Non-2xx/ {e += $NF}
       END {printf "%s %s %s %d\n", r + 0, p50 + 0, p99 + 0, e}' <<<"$out"
}

k=0
for b in "${BUILDS[@]}"; do
  label=${b%%=*} bin=${b#*=}
  for mode in "${MODES[@]}"; do
    k=$((k + 1)); first=$((3000 + 100 * k)); last=$((first + NPORTS - 1))
    env=(env -u TLS_CERT) scheme=http
    case $mode in https | h2tls) env=(env TLS_CERT="$TMP/cert.pem" TLS_KEY="$TMP/key.pem" GIONG_KTLS=off) scheme=https ;; esac
    tag=$label-$mode
    "${env[@]}" BENCH_HOST=$S BENCH_PORTS=$first-$last GIONG_MAX_CONNS=200000 GIONG_IDLE_TIMEOUT_SECS=7200 \
      taskset -c 0-7 "$bin" >/dev/null 2>"$OUT/raw/$tag-server.log" &
    pid=$!
    sleep 2
    read -r base_rps base_p50 base_p99 e0 <<<"$(probe "$scheme://$S:$first/json")"
    sleep 1
    rss0=$(kib $pid RssAnon) a0=$(avail) la0=$(on_l "awk '/^MemAvailable/{print \$2}' /proc/meminfo")
    cat /proc/net/sockstat >"$OUT/raw/$tag-sockstat-before.txt"
    log "c1m $tag: opening $CONNS connections to ports $first-$last"
    on_l "rm -f /root/c1m-ready.json"
    hpid=$(on_l "ulimit -Hn 2000000; ulimit -n 2000000; nohup /root/c1m-hold --addr $S --ports $first-$last --conns $CONNS --threads $HOLD_THREADS --mode $mode --ca /root/cert.pem --ready /root/c1m-ready.json </dev/null >/dev/null 2>/root/c1m-hold-$tag.log & echo \$!")
    for _ in $(seq 1200); do on_l "test -f /root/c1m-ready.json" && break; sleep 1; done
    ready=$(on_l "cat /root/c1m-ready.json" 2>/dev/null)
    echo "$ready" >"$OUT/raw/$tag-hold.json"
    sleep 10
    rss1=$(kib $pid RssAnon) vm1=$(kib $pid VmRSS) a1=$(avail) la1=$(on_l "awk '/^MemAvailable/{print \$2}' /proc/meminfo")
    cat /proc/net/sockstat >"$OUT/raw/$tag-sockstat-held.txt"
    on_l "cat /proc/net/sockstat" >"$OUT/raw/$tag-sockstat-held-br2.txt"
    ct=$(cat /proc/sys/net/netfilter/nf_conntrack_count 2>/dev/null || echo -)
    t0=$(ticks $pid); sleep 30; t1=$(ticks $pid)
    read -r held_rps held_p50 held_p99 e1 <<<"$(probe "$scheme://$S:$first/json")"
    ls /proc/$pid/fd | wc -l >"$OUT/raw/$tag-server-fds.txt"
    python3 - "$ready" "$label" "$mode" "$CONNS" "$rss0" "$rss1" "$vm1" "$a0" "$a1" "$la0" "$la1" "$OUT/raw/$tag-sockstat-held.txt" "$ct" \
      $((t1 - t0)) "$HZ" "$base_rps" "$base_p50" "$base_p99" "$held_rps" "$held_p50" "$held_p99" $((e0 + e1)) >>"$CSV" <<'PY'
import json, sys
(ready, label, mode, conns, rss0, rss1, vm1, a0, a1, la0, la1, sockstat, ct, ticks, hz,
 brps, b50, b99, hrps, h50, h99, perr) = sys.argv[1:]
try:
    j = json.loads(ready)
except Exception:
    j = {"held": 0, "errors": -1, "secs": 0, "per_s": 0}
held = j.get("held", 0) or 1
inuse = mem = 0
for line in open(sockstat):
    if line.startswith("TCP:"):
        f = line.split()
        inuse = int(f[f.index("inuse") + 1]); mem = int(f[f.index("mem") + 1])
srv_drop = (int(a0) - int(a1)) / 1024
cli_drop = (int(la0) - int(la1)) / 1024
print(f"{label},{mode},{conns},{j.get('held', 0)},{j.get('errors', 0)},{j.get('secs', 0)},{j.get('per_s', 0)},"
      f"{(int(rss1) - int(rss0)) / held:.2f},{int(vm1) / 1024:.0f},{srv_drop:.0f},{srv_drop * 1024 / held:.2f},"
      f"{cli_drop:.0f},{cli_drop * 1024 / held:.2f},{inuse},{mem * 4 / 1024:.0f},{ct},"
      f"{100 * int(ticks) / int(hz) / 30:.2f},{brps},{b50},{b99},{hrps},{h50},{h99},{perr}")
PY
    log "c1m $tag: $(tail -n1 "$CSV")"
    # Closing a million sockets takes a while: the client first, then the server.
    on_l "kill $hpid; for i in \$(seq 300); do kill -0 $hpid 2>/dev/null || break; sleep 1; done"
    on_l "cat /root/c1m-hold-$tag.log" >"$OUT/raw/$tag-hold.log" 2>&1
    kill $pid; wait $pid 2>/dev/null
    sleep 30
  done
done
python3 - "$CSV" <<'PY' | tee "$OUT/summary.txt"
import csv, sys
rows = list(csv.DictReader(l for l in open(sys.argv[1]) if not l.startswith("#")))
print(f"{'build':<7}{'mode':<7}{'held':>10}{'errors':>7}{'open s':>8}{'conn/s':>9}{'server KiB/conn':>16}{'+kernel':>9}"
      f"{'VmRSS MiB':>10}{'client KiB/conn':>16}{'idle CPU%':>10}{'probe req/s before/held':>26}{'p99 us before/held':>21}")
for r in rows:
    print(f"{r['build']:<7}{r['mode']:<7}{int(r['held']):>10,}{r['errors']:>7}{float(r['open_secs']):>8.1f}{float(r['open_per_s']):>9,.0f}"
          f"{float(r['server_rssanon_kib_per_conn']):>16.2f}{float(r['server_total_kib_per_conn']):>9.2f}{r['server_vmrss_mib']:>10}"
          f"{float(r['client_total_kib_per_conn']):>16.2f}{r['idle_cpu_pct']:>10}"
          f"{float(r['base_rps']):>13,.0f} / {float(r['held_rps']):<10,.0f}{r['base_p99']:>10} / {r['held_p99']:<8}")
print("server KiB/conn: growth of the server's RssAnon; +kernel: what the server machine lost in all (MemAvailable),")
print("socket buffers, conntrack and page tables included. idle CPU: the server over 30 s of quiet, 100 = one core.")
PY
