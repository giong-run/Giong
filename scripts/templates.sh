#!/usr/bin/env bash
# Builds and tests every project `giong new` can write, against this checkout's crates,
# the way a user would: fmt, clippy with warnings as errors, the tests on io_uring and
# on epoll. CI's `templates` job runs it on every pull request, so a template cannot rot.
#
#   scripts/templates.sh              every combination
#   scripts/templates.sh web-pg       the combinations whose name contains "web-pg"
#   scripts/templates.sh --list       print the combinations
#
# The database tests read DATABASE_URL, MYSQL_URL and REDIS_URL (MYSQL_URL becomes the
# DATABASE_URL of the mysql projects). The sqlite projects need no server: their tests
# use files of their own, and their service a file under $GIONG_TEMPLATES_DIR. Without them a project's database tests print
# SKIPPED and pass, and its service is not started (it needs its database); with
# GIONG_TEMPLATES_REQUIRE_DB=1 (as in CI) a SKIPPED fails the run.
# Projects are written under $GIONG_TEMPLATES_DIR (default: a temporary directory) and
# share one target directory, so each one only compiles its own code.
#
# On Windows (Git Bash) the same steps run, without the epoll test run (there is no epoll),
# and the smoke run stops the service with kill, as Windows has no SIGTERM: the graceful
# stop on Windows is the runtime's own tests' to check. GIONG_TEMPLATES_NO_SERVERS=1 runs
# only the combinations that need no database server (the Windows runners have none).
set -uo pipefail
cd "$(dirname "$0")/.."
ROOT=$(pwd)

# name           giong new flags
COMBOS="
web              --template web --db none
web-tls          --template web --db none --tls
web-sqlite       --template web
web-sqlite-redis-tls --template web --db sqlite,redis --tls
web-pg           --template web --db postgres
web-pg-redis-tls --template web --db postgres,redis --tls
web-mysql        --template web --db mysql
web-mysql-redis  --template web --db mysql,redis
web-redis        --template web --db redis
grpc             --template grpc
grpc-tls         --template grpc --tls
worker           --template worker
worker-pg        --template worker --db postgres
worker-mysql     --template worker --db mysql
worker-redis     --template worker --db redis
"

if [ "${1:-}" = "--list" ]; then
    echo "$COMBOS" | awk 'NF { print $1 }'
    exit 0
fi
FILTER="${1:-}"

case "$(uname -s)" in
    MINGW* | MSYS* | CYGWIN*) WINDOWS=1 EXE=.exe ;;
    *) WINDOWS=0 EXE= ;;
esac

WORK="${GIONG_TEMPLATES_DIR:-$(mktemp -d)}"
mkdir -p "$WORK"
if [ "$WINDOWS" = 1 ]; then
    # C:/... rather than /tmp/...: the paths reach programs that are not Git Bash's
    # (giong, a sqlite: URL the service opens).
    WORK=$(cygpath -m "$WORK")
    ROOT=$(cygpath -m "$ROOT")
fi
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$WORK/target}"
cargo build -q -p giong-cli || exit 1
GIONG="$CARGO_TARGET_DIR/debug/giong$EXE"

# A port for a smoke run: below the range the kernel hands out to outgoing
# connections (ip_local_port_range, 32768-60999 by default), which the
# database clients of the same job take, and with nothing listening on it.
free_port() {
    local low=32768 port
    read -r low _ </proc/sys/net/ipv4/ip_local_port_range 2>/dev/null || low=32768
    [ "$low" -gt 12000 ] || low=32768
    for _ in 1 2 3 4 5 6 7 8 9 10; do
        port=$((10000 + RANDOM % (low - 10000)))
        if ! (exec 3<>"/dev/tcp/127.0.0.1/$port") 2>/dev/null; then
            echo "$port"; return 0
        fi
    done
    echo "$port"
}

# Runs the service's binary, waits until it serves (or, for a worker,
# runs a job), then sends SIGTERM and expects a clean exit within the grace period.
# A port taken between the pick and the bind (AddrInUse) gets another try.
smoke() {
    local status
    for _ in 1 2 3; do
        status=0
        smoke_once "$@" || status=$?
        [ "$status" = 2 ] || return "$status"
        echo "smoke: $1: the port was taken, trying another"
    done
    return 1
}

smoke_once() {
    local name="$1"; shift
    local port out="$WORK/$name.run" pid deadline
    port=$(free_port)
    cargo build -q || return 1
    # shellcheck disable=SC2086
    "$@" env LISTEN="127.0.0.1:$port" WORKERS=2 INTERVAL_SECS=1 GIONG_MCP= \
        "$CARGO_TARGET_DIR/debug/${name}$EXE" >"$out" 2>&1 &
    pid=$!
    local start=$SECONDS
    deadline=$((SECONDS + 30))
    until case "$name" in
            web*) curl -fsS "http://127.0.0.1:$port/health" >/dev/null 2>&1 ;;
            grpc*) (exec 3<>"/dev/tcp/127.0.0.1/$port") 2>/dev/null ;;
            # A queue worker logs only when it runs a job: up for 3 s (its
            # tables made, its store reached) is the sign.
            worker-*) [ $SECONDS -ge $((start + 3)) ] ;;
            worker) grep -q '"event":"tick"' "$out" ;;
          esac; do
        if ! kill -0 "$pid" 2>/dev/null || [ $SECONDS -ge $deadline ]; then
            kill "$pid" 2>/dev/null
            if grep -q 'AddrInUse' "$out"; then
                return 2
            fi
            echo "smoke: $name did not start serving:"; cat "$out"
            return 1
        fi
        sleep 0.2
    done
    case "$name" in
        web*pg*|web*mysql*|web*sqlite*)
            curl -fsS -o /dev/null "http://127.0.0.1:$port/ready" || { cat "$out"; kill "$pid"; return 1; } ;;
    esac
    if [ "$WINDOWS" = 1 ]; then
        kill "$pid"
        wait "$pid" 2>/dev/null
        echo "smoke: $name served (stopped with kill: Windows has no SIGTERM)"
        return 0
    fi
    kill -TERM "$pid"
    local status=0
    wait "$pid" || status=$?
    if [ "$status" != 0 ]; then
        echo "smoke: $name exited with $status after SIGTERM:"; cat "$out"; return 1
    fi
    echo "smoke: $name served and stopped cleanly"
}

failed=()
while read -r name flags; do
    [ -n "$name" ] || continue
    case "$name" in *"$FILTER"*) ;; *) continue ;; esac
    if [ "${GIONG_TEMPLATES_NO_SERVERS:-0}" = 1 ]; then
        case "$flags" in *postgres* | *mysql* | *redis*) continue ;; esac
    fi
    dir="$WORK/$name"
    rm -rf "$dir"
    echo "=== $name: giong new $flags"
    # shellcheck disable=SC2086
    if ! "$GIONG" new "$dir" $flags --giong-path "$ROOT" --no-git >/dev/null; then
        failed+=("$name (giong new)"); continue
    fi
    url_env=()
    case "$name" in
        *mysql*) url_env=(env "DATABASE_URL=${MYSQL_URL:-}") ;;
        # The default database of the web template, so "web-sqlite" passes no --db.
        *sqlite*) url_env=(env "DATABASE_URL=sqlite:$WORK/$name-data/app.db") ;;
    esac
    # The service needs its database to start: without one, no smoke run.
    missing=""
    case "$flags" in *postgres*) [ -n "${DATABASE_URL:-}" ] || missing="DATABASE_URL" ;; esac
    case "$flags" in *mysql*) [ -n "${MYSQL_URL:-}" ] || missing="MYSQL_URL" ;; esac
    case "$flags" in *redis*) [ -n "${REDIS_URL:-}" ] || missing="${missing:+$missing and }REDIS_URL" ;; esac
    log="$WORK/$name.log"
    ok=1
    (
        cd "$dir" || exit 1
        set -e
        cargo fmt --check
        cargo clippy -q --all-targets -- -D warnings
        "${url_env[@]}" cargo test -q -- --show-output 2>&1
        if [ "$WINDOWS" = 0 ]; then
            "${url_env[@]}" env GIONG_DRIVER=epoll cargo test -q -- --show-output 2>&1
        fi
        if [ -n "$missing" ]; then
            # Its tests printed SKIPPED for the same reason.
            echo "smoke: SKIPPED for $name: $missing is not set"
        else
            smoke "$name" "${url_env[@]}"
        fi
    ) >"$log" 2>&1 || ok=0
    if [ "$ok" = 1 ] && [ "${GIONG_TEMPLATES_REQUIRE_DB:-0}" = 1 ] && grep -q SKIPPED "$log"; then
        echo "a test was skipped, but GIONG_TEMPLATES_REQUIRE_DB=1:"
        grep SKIPPED "$log"
        ok=0
    fi
    if [ "$ok" = 1 ]; then
        echo "ok: $name ($(grep -c '^test result: ok' "$log") test runs$(grep -q SKIPPED "$log" && echo ', some SKIPPED'))"
        grep '^smoke:' "$log"
    else
        tail -40 "$log"
        failed+=("$name")
    fi
done <<< "$COMBOS"

if [ ${#failed[@]} -gt 0 ]; then
    echo "FAILED: ${failed[*]}"
    exit 1
fi
echo "every template passed"
