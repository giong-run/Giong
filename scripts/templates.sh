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
# DATABASE_URL of the mysql projects). Without them a project's database tests print
# SKIPPED and pass; with GIONG_TEMPLATES_REQUIRE_DB=1 (as in CI) a SKIPPED fails the run.
# Projects are written under $GIONG_TEMPLATES_DIR (default: a temporary directory) and
# share one target directory, so each one only compiles its own code.
set -uo pipefail
cd "$(dirname "$0")/.."
ROOT=$(pwd)

# name           giong new flags
COMBOS="
web              --template web
web-tls          --template web --tls
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

WORK="${GIONG_TEMPLATES_DIR:-$(mktemp -d)}"
mkdir -p "$WORK"
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$WORK/target}"
cargo build -q -p giong-cli || exit 1
GIONG="$CARGO_TARGET_DIR/debug/giong"

failed=()
while read -r name flags; do
    [ -n "$name" ] || continue
    case "$name" in *"$FILTER"*) ;; *) continue ;; esac
    dir="$WORK/$name"
    rm -rf "$dir"
    echo "=== $name: giong new $flags"
    # shellcheck disable=SC2086
    if ! "$GIONG" new "$dir" $flags --giong-path "$ROOT" --no-git >/dev/null; then
        failed+=("$name (giong new)"); continue
    fi
    url_env=()
    case "$flags" in
        *mysql*) url_env=(env "DATABASE_URL=${MYSQL_URL:-}") ;;
    esac
    log="$WORK/$name.log"
    ok=1
    (
        cd "$dir" || exit 1
        set -e
        cargo fmt --check
        cargo clippy -q --all-targets -- -D warnings
        "${url_env[@]}" cargo test -q -- --show-output 2>&1
        "${url_env[@]}" env GIONG_DRIVER=epoll cargo test -q -- --show-output 2>&1
    ) >"$log" 2>&1 || ok=0
    if [ "$ok" = 1 ] && [ "${GIONG_TEMPLATES_REQUIRE_DB:-0}" = 1 ] && grep -q SKIPPED "$log"; then
        echo "a test was skipped, but GIONG_TEMPLATES_REQUIRE_DB=1:"
        grep SKIPPED "$log"
        ok=0
    fi
    if [ "$ok" = 1 ]; then
        echo "ok: $name ($(grep -c '^test result: ok' "$log") test runs$(grep -q SKIPPED "$log" && echo ', some SKIPPED'))"
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
