#!/usr/bin/env bash
# The checks a branch needs before it is pushed, chosen from the files it changes.
#
#   scripts/preflight.sh                 plan, then run the quick checks
#   scripts/preflight.sh --plan          only print what changed and what it needs
#   scripts/preflight.sh --deep          also run the slow checks the change calls for
#                                        (ThreadSanitizer, loom, a minute of each fuzz
#                                        target, the minimum Rust version)
#   scripts/preflight.sh --against 172,173
#                                        also check that the branch merges without a
#                                        conflict with those open pull requests, so the
#                                        merge queue does not eject one of them
#   scripts/preflight.sh --base <ref>    compare with <ref> instead of origin/main
#
# A red CI run costs about 20 minutes before anyone sees it, and another run after the
# fix; this script catches the usual causes (formatting, clippy, a doc link, CHANGELOG
# form, a test of the crate that changed, a conflict with main) in a few minutes. It runs
# a subset of what CI runs and never replaces it: the merge queue still runs every job.
# Exit status: 0 when every check it ran passed.
set -uo pipefail
cd "$(dirname "$0")/.."

BASE=origin/main
AGAINST=""
MODE=quick
while [ $# -gt 0 ]; do
    case "$1" in
        --plan) MODE=plan ;;
        --deep) MODE=deep ;;
        --base) BASE="$2"; shift ;;
        --against) AGAINST="$2"; shift ;;
        -h|--help) sed -n '2,22p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
        *) echo "unknown argument: $1 (see --help)" >&2; exit 2 ;;
    esac
    shift
done

if ! git rev-parse --verify --quiet "$BASE^{commit}" > /dev/null; then
    echo "error: $BASE does not exist here; run 'git fetch origin main' or pass --base" >&2
    exit 2
fi
FORK=$(git merge-base "$BASE" HEAD)
# Committed changes since the fork point, plus whatever is not committed yet.
mapfile -t FILES < <({ git diff --name-only "$FORK" HEAD; git diff --name-only HEAD; \
    git ls-files --others --exclude-standard; } | sort -u)

if [ ${#FILES[@]} -eq 0 ]; then
    echo "No change since $BASE ($(git rev-parse --short "$FORK"))."
    exit 0
fi

# ------------------------------------------------------------------ classify
declare -A CRATES=()   # workspace packages whose code or tests changed
RUST=0                 # anything cargo builds
SOAK=()                # files on a soak path (docs/RELEASING.md, "How long to soak")
UNSAFE_CORE=0          # code ThreadSanitizer, valgrind and Kani watch
NOTIFY=0               # the loom-modelled wake-ups
CHANGELOG_ENTRY=0
USER_VISIBLE=0
declare -A FUZZ=()     # crates with fuzz targets whose parsing code changed
OTHER_WS=()            # bench/ and sim/ are workspaces of their own

for f in "${FILES[@]}"; do
    case "$f" in
        changelog.d/README.md) ;;
        changelog.d/*.md|CHANGELOG.md) CHANGELOG_ENTRY=1 ;;
    esac
    case "$f" in
        bench/*|sim/*) OTHER_WS+=("$f"); continue ;;
    esac
    case "$f" in
        *.rs|Cargo.toml|*/Cargo.toml|Cargo.lock|build.rs) RUST=1 ;;
    esac
    case "$f" in
        src/*|tests/*|Cargo.toml|build.rs|fuzz/*) CRATES[giong]=1 ;;
        examples/rest-api/*) CRATES[rest-api]=1 ;;
        giong-*/*) CRATES[${f%%/*}]=1 ;;
    esac
    case "$f" in
        src/*.rs|giong-*/src/*.rs) USER_VISIBLE=1 ;;
    esac
    case "$f" in
        src/driver/*|src/op.rs|src/buf.rs|src/kbox.rs|src/slab.rs|src/runtime.rs|src/executor.rs|\
        src/time.rs|src/net.rs|src/net/*|src/sync/*|src/sync.rs|src/fs.rs|giong-tls/src/*)
            SOAK+=("$f") ;;
        giong-http/src/client/*) ;;
        giong-http/src/*) SOAK+=("$f") ;;
    esac
    case "$f" in
        src/driver/*|src/op.rs|src/buf.rs|src/kbox.rs|src/slab.rs|src/notify.rs|src/executor.rs) UNSAFE_CORE=1 ;;
    esac
    case "$f" in
        src/notify.rs) NOTIFY=1 ;;
    esac
    case "$f" in
        src/net/dns.rs|src/net/dns/*|fuzz/*) FUZZ[giong]=1 ;;
        giong-http/src/*|giong-http/fuzz/*) FUZZ[giong-http]=1 ;;
        giong-tls/src/ktls*|giong-tls/fuzz/*) FUZZ[giong-tls]=1 ;;
        giong-postgres/src/*|giong-mysql/src/*|giong-redis/src/*|giong-web/src/*|giong-mcp/src/*)
            [ -d "${f%%/*}/fuzz" ] && FUZZ[${f%%/*}]=1 ;;
    esac
done
# The runtime's code reaches every crate built on it.
ALL=0
for f in "${FILES[@]}"; do
    case "$f" in src/*|Cargo.toml|Cargo.lock) ALL=1 ;; esac
done

# ------------------------------------------------------------------ plan
echo "== $((${#FILES[@]})) files changed since $BASE ($(git rev-parse --short "$FORK"))"
printf '   %s\n' "${FILES[@]:0:40}"
[ ${#FILES[@]} -gt 40 ] && echo "   ... and $((${#FILES[@]} - 40)) more"
echo
if [ "$RUST" = 0 ]; then
    echo "Docs or configuration only: no cargo build needed here."
elif [ "$ALL" = 1 ]; then
    echo "Tests: the whole workspace, on io_uring and on epoll (the runtime changed)."
else
    echo "Tests: ${!CRATES[*]}"
fi
if [ ${#SOAK[@]} -gt 0 ]; then
    echo "Soak: on a soak path, so a release that contains it needs 2 to 4 hours per driver"
    echo "      (unless only doc comments changed): ${SOAK[*]:0:6}"
else
    echo "Soak: none needed for this change."
fi
[ ${#OTHER_WS[@]} -gt 0 ] && echo "bench/ or sim/ changed: they are separate workspaces; build them where they changed."
if [ "$USER_VISIBLE" = 1 ] && [ "$CHANGELOG_ENTRY" = 0 ]; then
    echo "Changelog: library code changed and no changelog.d/ entry was added. Add one if a"
    echo "           user would notice (changelog.d/README.md); skip it for internal changes."
fi
SLOW=()
[ "$UNSAFE_CORE" = 1 ] && SLOW+=("scripts/check.sh tsan")
[ "$NOTIFY" = 1 ] && SLOW+=("scripts/check.sh loom")
[ "$RUST" = 1 ] && SLOW+=("cargo +1.80.0 check --workspace --all-features")
for c in "${!FUZZ[@]}"; do SLOW+=("fuzz: $c (a minute per target)"); done
if [ ${#SLOW[@]} -gt 0 ]; then
    echo "Slow checks this change calls for (--deep runs them):"
    printf '   %s\n' "${SLOW[@]}"
fi
echo
[ "$MODE" = plan ] && exit 0

# ------------------------------------------------------------------ run
FAILED=()
PASSED=()
step() {
    local name="$1"; shift
    echo "== $name"
    if "$@"; then PASSED+=("$name"); else FAILED+=("$name"); echo "!! $name failed"; fi
}

merges_cleanly() {
    local other="$1" label="$2" out
    if out=$(git merge-tree --write-tree --name-only --no-messages "$other" HEAD 2>&1); then
        return 0
    fi
    echo "conflicts with $label in:"
    echo "$out" | tail -n +2 | sed 's/^/   /'
    return 1
}
step "merges into $BASE" merges_cleanly "$BASE" "$BASE"
if [ -n "$AGAINST" ]; then
    IFS=, read -ra PRS <<< "$AGAINST"
    for n in "${PRS[@]}"; do
        if git fetch -q origin "pull/$n/head" 2>/dev/null; then
            step "merges with #$n" merges_cleanly FETCH_HEAD "pull request #$n"
        else
            FAILED+=("fetch #$n"); echo "!! could not fetch pull/$n/head"
        fi
    done
fi

step "release.py check" python3 scripts/release.py check
if [ "$RUST" = 1 ]; then
    step "fmt" cargo fmt --all --check
    step "clippy" cargo clippy --workspace --all-targets --all-features --quiet -- -D warnings
    step "doc" env RUSTDOCFLAGS="-D warnings" cargo doc --workspace --all-features --no-deps --quiet
    if [ "$ALL" = 1 ]; then
        step "tests (io_uring)" cargo test --workspace --all-features --quiet
        step "tests (epoll)" env GIONG_DRIVER=epoll cargo test --workspace --all-features --quiet
    else
        for c in "${!CRATES[@]}"; do
            step "tests $c" cargo test -p "$c" --all-features --quiet
        done
        [ -n "${CRATES[giong]:-}" ] && \
            step "tests giong (epoll)" env GIONG_DRIVER=epoll cargo test -p giong --all-features --quiet
    fi
fi

if [ "$MODE" = deep ]; then
    [ "$UNSAFE_CORE" = 1 ] && step "tsan" scripts/check.sh tsan
    [ "$NOTIFY" = 1 ] && step "loom" scripts/check.sh loom
    [ "$RUST" = 1 ] && step "msrv" cargo +1.80.0 check --workspace --all-features --quiet
    for c in "${!FUZZ[@]}"; do
        dir=.; [ "$c" != giong ] && dir="$c"
        for t in $(cd "$dir" && cargo +nightly fuzz list 2>/dev/null); do
            step "fuzz $c/$t" bash -c "cd '$dir' && cargo +nightly fuzz run --target x86_64-unknown-linux-gnu '$t' -- -max_total_time=60"
        done
    done
fi

echo
echo "== passed: ${#PASSED[@]}, failed: ${#FAILED[@]}"
if [ ${#FAILED[@]} -gt 0 ]; then
    printf '   failed: %s\n' "${FAILED[@]}"
    exit 1
fi
echo "CI still runs every job (kernels, fallback tiers, TSan, Miri, Kani, semver, instr) on the pull request and in the merge queue."
