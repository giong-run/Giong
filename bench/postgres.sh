#!/usr/bin/env bash
# PostgreSQL client benchmark: giong-postgres against tokio-postgres, each on
# one core, against the same local PostgreSQL server.
#
#   bench/postgres.sh [rounds] [seconds-per-measurement]
#
# Method
#   * A throwaway cluster (initdb in a temporary directory, fsync off) on
#     SERVER_CPUS, a table of 10 000 users. The client runs on CLIENT_CPU, so
#     the numbers show how much one core of client gets done.
#   * Workloads: `point` reads one row by primary key, `list` reads 100 rows;
#     every column of every row is decoded into Rust values (i64, &str, &str,
#     SystemTime).
#   * Modes: `pool` is the usual service setup, POOL_SIZE connections (4 by
#     default): giong's Pool (statements share connections and pipeline)
#     against deadpool-postgres with prepare_cached (a connection per
#     statement while it runs). `client` is one connection shared by every
#     task, which both libraries pipeline.
#   * TASKS concurrent tasks issue statements back to back. 2 s of warm-up,
#     then SECS measured. `cpu%` is the client's core; `srv%` the server's
#     cores together (100 per core), from /proc/stat. A client below 100% was
#     waiting for the server.
#
# WORKLOADS, MODES and CLIENTS pick a subset, for example
#   POOL_SIZE=16 WORKLOADS=point MODES=pool CLIENTS=tokio OUT=results/x bench/postgres.sh 3
#
# Requires: PostgreSQL server binaries (initdb, postgres, psql), cargo,
# python3. Results: bench/results/postgres/.
set -euo pipefail
cd "$(dirname "$0")"

ROUNDS=${1:-5}
SECS=${2:-5}
CLIENT_CPU=${CLIENT_CPU:-3}
SERVER_CPUS=${SERVER_CPUS:-0,1,2}
TASKS=${TASKS:-64}
export POOL_SIZE=${POOL_SIZE:-4}
WORKLOADS=${WORKLOADS:-point list}
MODES=${MODES:-pool client}
CLIENTS=${CLIENTS:-giong tokio}
OUT=${OUT:-results/postgres}
mkdir -p "$OUT"
CSV="$OUT/raw.csv"
export CLK_TCK=$(getconf CLK_TCK)

BIN=${PG_BIN:-$(ls -d /usr/lib/postgresql/*/bin 2>/dev/null | sort -V | tail -1)}
[ -x "$BIN/initdb" ] || { echo "no PostgreSQL binaries; set PG_BIN" >&2; exit 2; }

echo "building..." >&2
for d in postgres-giong postgres-tokio; do (cd "$d" && cargo build --release -q); done

TMP=$(mktemp -d)
AS=()
if [ "$(id -u)" = 0 ]; then
  chown postgres "$TMP"
  AS=(runuser -u postgres --)
fi
cleanup() {
  if [ -n "${PG_PID:-}" ]; then
    kill "$PG_PID" 2>/dev/null || true
    wait "$PG_PID" 2>/dev/null || true
  fi
  rm -rf "$TMP"
}
trap cleanup EXIT
PORT=55432
"${AS[@]}" "$BIN/initdb" -D "$TMP/data" -U postgres -A trust -E UTF8 --locale=C --no-sync >/dev/null
cat >> "$TMP/data/postgresql.conf" <<CONF
listen_addresses = '127.0.0.1'
port = $PORT
unix_socket_directories = '$TMP'
fsync = off
synchronous_commit = off
shared_buffers = 256MB
max_connections = 100
CONF
taskset -c "$SERVER_CPUS" "${AS[@]}" "$BIN/postgres" -D "$TMP/data" >"$TMP/server.log" 2>&1 &
PG_PID=$!
for _ in $(seq 100); do "$BIN/pg_isready" -q -h 127.0.0.1 -p "$PORT" && break; sleep 0.1; done
"$BIN/psql" -q -h 127.0.0.1 -p "$PORT" -U postgres -c "
  CREATE TABLE bench_users (id bigint PRIMARY KEY, name text NOT NULL, email text NOT NULL, created_at timestamptz NOT NULL);
  INSERT INTO bench_users SELECT i, 'user ' || i, 'user' || i || '@example.com', now() - i * interval '1 minute' FROM generate_series(1, 10000) i;
  ANALYZE bench_users;" >/dev/null
URL="postgres://postgres@127.0.0.1:$PORT/postgres"

# Busy ticks of the server's cores, from /proc/stat.
server_ticks() {
  awk -v cpus=",$SERVER_CPUS," '$1 ~ /^cpu[0-9]+$/ && index(cpus, "," substr($1, 4) ",") {
    busy += $2 + $3 + $4 + $7 + $8 } END { print busy }' /proc/stat
}

echo "client,workload,mode,round,ops,p50,p99,cpu,srv" > "$CSV"
for round in $(seq 1 "$ROUNDS"); do
  for workload in $WORKLOADS; do
    for mode in $MODES; do
      for client in $CLIENTS; do
        out_file="$TMP/out"
        taskset -c "$CLIENT_CPU" "postgres-$client/target/release/bench-postgres-$client" \
          "$URL" "$workload" "$mode" "$TASKS" "$SECS" >"$out_file" &
        cpid=$!
        sleep 2.2
        before=$(server_ticks)
        sleep "$SECS"
        after=$(server_ticks)
        wait "$cpid"
        out=$(cat "$out_file")
        srv=$(( (after - before) * 100 / (CLK_TCK * SECS) ))
        get() { sed -n "s/.*$1=\([0-9.]*\).*/\1/p" <<<"$out"; }
        echo "$client,$workload,$mode,$round,$(get ops),$(get p50_us),$(get p99_us),$(get cpu),$srv" >> "$CSV"
      done
    done
  done
  echo "round $round/$ROUNDS done" >&2
done

python3 - "$CSV" "$TASKS" "$POOL_SIZE" "$ROUNDS" "$SECS" <<'PY' | tee "$OUT/summary.txt"
import csv, statistics, sys
rows = list(csv.DictReader(open(sys.argv[1])))
med = statistics.median
print(f"{sys.argv[2]} tasks, pool of {sys.argv[3]}, {sys.argv[4]} rounds of {sys.argv[5]} s; median, min and max of the rounds")
print(f"{'workload':<9}{'mode':<8}{'client':<8}{'ops/s median':>13}{'min':>9}{'max':>9}{'p50 us':>8}{'p99 us':>8}{'cpu%':>6}{'srv%':>6}")
for w in ("point", "list"):
    for m in ("pool", "client"):
        for c in ("giong", "tokio"):
            sel = [r for r in rows if (r["workload"], r["mode"], r["client"]) == (w, m, c)]
            if not sel: continue
            ops = [float(r["ops"]) for r in sel]
            print(f"{w:<9}{m:<8}{c:<8}{med(ops):13.0f}{min(ops):9.0f}{max(ops):9.0f}"
                  f"{med(float(r['p50']) for r in sel):8.0f}{med(float(r['p99']) for r in sel):8.0f}"
                  f"{med(float(r['cpu']) for r in sel):6.0f}{med(float(r['srv']) for r in sel):6.0f}")
        print()
PY
