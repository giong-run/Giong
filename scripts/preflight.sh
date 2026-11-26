#!/usr/bin/env bash
# The gate a branch passes before it is pushed: every check CI runs that the files it
# changes call for, chosen from those files, plus a merge check against main and, on
# request, against open pull requests.
#
#   scripts/preflight.sh                 the gate: run everything the change calls for
#   scripts/preflight.sh --quick         while iterating: merge check, release.py check,
#                                        fmt, clippy, docs and the tests of the crates
#                                        that changed on the default driver
#   scripts/preflight.sh --plan          only print what changed and what it needs
#   scripts/preflight.sh --against 172,173
#                                        also merge with those open pull requests and
#                                        name the files that conflict
#   scripts/preflight.sh --base <ref>    compare with <ref> instead of origin/main
#   scripts/preflight.sh --install       install the toolchains and tools the gate uses
#   scripts/preflight.sh --install-hook  make `git push` run the quick gate first (a
#                                        pre-push hook in this clone); every agent and
#                                        person installs it once per clone
#
# What the gate runs, by what changed:
#   always                    merge into the base, scripts/release.py check
#   any Rust or Cargo file    fmt, clippy -D warnings, docs -D warnings, scripts/msrv.sh,
#                             the tests on io_uring and on epoll: of the crates that
#                             changed, or of the whole workspace when the runtime did
#   a published crate's src   scripts/semver.sh (no breaking change since the release)
#   unsafe code, the drivers, buffers, the executor, anything that crosses threads
#                             scripts/check.sh tsan, one round
#   wake-ups (src/notify.rs, the executor's injector)
#                             scripts/check.sh loom
#   code that parses bytes from the network
#                             every fuzz target of that crate for FUZZ_SECS (30) seconds
#   a Cargo.toml, Cargo.lock or deny.toml
#                             cargo deny --workspace check: no RustSec advisory or yanked
#                             crate, permissive licenses, crates.io only (deny.toml)
# A missing tool fails the gate with the command that installs it; nothing is skipped
# quietly. CI runs the fast set on a pull request and every job, repeated, in the merge
# queue (kernels, fallback tiers, Miri, Kani, instr, the server versions, macOS, Windows;
# docs/WORKFLOW.md), so the gate makes a green queue run likely; it does not replace it.
# Exit status: 0 when every check it ran passed.
set -uo pipefail
cd "$(dirname "$0")/.."

BASE=origin/main
AGAINST=""
MODE=gate
FUZZ_SECS="${FUZZ_SECS:-30}"
MSRV=$(scripts/msrv.sh versions | head -n 1)   # every MSRV toolchain: scripts/msrv.sh
SEMVER_CHECKS=0.51.0   # the version CI installs (.github/workflows/ci.yml, job semver)
CARGO_DENY=0.22.1      # the version CI installs (.github/workflows/ci.yml, job lint)
# The stable compiler CI pins (RUST_STABLE in .github/workflows/ci.yml): clippy's lints
# and cargo's behaviour change between releases, so the gate runs the same one.
STABLE=$(sed -n 's/^  RUST_STABLE: "\(.*\)"$/\1/p' .github/workflows/ci.yml | head -n 1)

install_tools() {
    set -e
    rustup toolchain install "$STABLE" --profile minimal -c rustfmt -c clippy
    for v in $(scripts/msrv.sh versions); do rustup toolchain install "$v" --profile minimal; done
    rustup toolchain install nightly --profile minimal -c rust-src
    rustup component add rustfmt clippy
    cargo install --locked cargo-fuzz
    cargo install --locked cargo-semver-checks --version "$SEMVER_CHECKS"
    cargo install --locked cargo-deny --version "$CARGO_DENY"
    echo "ok: $STABLE (CI's stable), $MSRV, nightly with rust-src, cargo-fuzz, cargo-semver-checks $SEMVER_CHECKS and cargo-deny $CARGO_DENY"
}

install_hook() {
    local hook
    hook="$(git rev-parse --git-path hooks)/pre-push"
    mkdir -p "$(dirname "$hook")"
    cat > "$hook" <<'HOOK'
#!/usr/bin/env bash
# Installed by scripts/preflight.sh --install-hook: the quick gate before every push,
# so that a run of CI is not spent on what a local check finds in a minute.
# A push that only deletes refs, or a push of main itself, skips it.
zero=0000000000000000000000000000000000000000
run=0
while read -r _local_ref local_sha remote_ref _remote_sha; do
    [ "$local_sha" = "$zero" ] && continue
    [ "$remote_ref" = refs/heads/main ] && continue
    run=1
done
[ "$run" = 1 ] || exit 0
exec "$(git rev-parse --show-toplevel)/scripts/preflight.sh" --quick
HOOK
    chmod +x "$hook"
    echo "ok: $hook runs scripts/preflight.sh --quick before every push"
}

while [ $# -gt 0 ]; do
    case "$1" in
        --install-hook) install_hook; exit $? ;;
        --plan) MODE=plan ;;
        --quick) MODE=quick ;;
        --base) BASE="$2"; shift ;;
        --against) AGAINST="$2"; shift ;;
        --install) install_tools; exit $? ;;
        -h|--help) sed -n '2,36p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
        *) echo "unknown argument: $1 (see --help)" >&2; exit 2 ;;
    esac
    shift
done

# Every cargo call below that names no toolchain uses CI's stable compiler.
if [ -n "$STABLE" ] && rustup run "$STABLE" rustc --version > /dev/null 2>&1; then
    export RUSTUP_TOOLCHAIN="$STABLE"
elif [ -n "$STABLE" ]; then
    echo "warning: CI builds with Rust $STABLE, which is not installed here ($(rustc --version 2>/dev/null));" \
        "clippy and the tests may disagree with CI. Install it: scripts/preflight.sh --install" >&2
fi

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
declare -A FUZZ=()     # crate directories with fuzz targets whose parsing code changed
RUST=0                 # anything cargo builds
ALL=0                  # the runtime changed: every crate is built on it
SEMVER=0               # a published crate's library code changed
TSAN=0                 # ThreadSanitizer applies
LOOM=0                 # the loom models apply
DEPS=0                 # dependencies or the supply-chain policy changed
SOAK=()                # files on a soak path (docs/RELEASING.md, "How long to soak")
CHANGELOG_ENTRY=0
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
        *.rs|Cargo.toml|*/Cargo.toml|Cargo.lock) RUST=1 ;;
    esac
    case "$f" in
        Cargo.toml|*/Cargo.toml|Cargo.lock|deny.toml) DEPS=1 ;;
    esac
    case "$f" in
        src/*|Cargo.toml|Cargo.lock) ALL=1 ;;
    esac
    case "$f" in
        src/*|tests/*|Cargo.toml|fuzz/*) CRATES[giong]=1 ;;
        examples/rest-api/*) CRATES[rest-api]=1 ;;
        giong-*/*) CRATES[${f%%/*}]=1 ;;
    esac
    case "$f" in
        src/*.rs|giong-*/src/*.rs) SEMVER=1 ;;
    esac
    case "$f" in
        src/driver/*|src/op.rs|src/buf.rs|src/kbox.rs|src/slab.rs|src/runtime.rs|src/executor.rs|\
        src/time.rs|src/net.rs|src/net/*|src/sync/*|src/sync.rs|src/fs.rs|giong-tls/src/*)
            SOAK+=("$f") ;;
        giong-http/src/client/*) ;;
        giong-http/src/*) SOAK+=("$f") ;;
    esac
    case "$f" in
        src/driver/*|src/op.rs|src/buf.rs|src/kbox.rs|src/slab.rs|src/notify.rs|src/executor.rs|\
        src/runtime.rs|src/sync/*|src/sync.rs|src/observe.rs) TSAN=1 ;;
    esac
    case "$f" in
        src/notify.rs|src/executor.rs) LOOM=1 ;;
    esac
    case "$f" in
        src/net/dns.rs|src/net/dns/*|fuzz/*) FUZZ[.]=1 ;;
        giong-http/src/*|giong-http/fuzz/*) FUZZ[giong-http]=1 ;;
        giong-tls/src/ktls*|giong-tls/fuzz/*) FUZZ[giong-tls]=1 ;;
        giong-*/src/*|giong-*/fuzz/*) [ -d "${f%%/*}/fuzz" ] && FUZZ[${f%%/*}]=1 ;;
    esac
done
# New or changed unsafe code anywhere is ThreadSanitizer's business too.
if git diff "$FORK" -- '*.rs' | grep -qE '^\+.*\bunsafe\b'; then
    TSAN=1
fi
[ "$RUST" = 0 ] && SEMVER=0
[ ${#CRATES[@]} -eq 0 ] && [ "$RUST" = 1 ] && ALL=1

# ------------------------------------------------------------------ plan
echo "== ${#FILES[@]} files changed since $BASE ($(git rev-parse --short "$FORK"))"
printf '   %s\n' "${FILES[@]:0:40}"
[ ${#FILES[@]} -gt 40 ] && echo "   ... and $((${#FILES[@]} - 40)) more"
echo
CI_SCOPE=$(printf '%s\n' "${FILES[@]}" | python3 scripts/ci-scope.py 2>/dev/null | sed -n 's/^scope=//p')
case "$CI_SCOPE" in
    docs) echo "CI on the pull request: lint and msrv only (no file a build or test reads changed)." ;;
    pr) echo "CI on the pull request: the fast set (build, tests once on io_uring and epoll); the merge queue runs every job, repeated, macOS and Windows included (docs/WORKFLOW.md)." ;;
esac
if [ "$RUST" = 0 ]; then
    echo "Docs, scripts or configuration only: no cargo build needed."
elif [ "$ALL" = 1 ]; then
    echo "Tests: the whole workspace, on io_uring and on epoll (the runtime changed)."
else
    echo "Tests: ${!CRATES[*]}, on io_uring and on epoll."
fi
GATE=()
[ "$RUST" = 1 ] && GATE+=("MSRV checks (scripts/msrv.sh: $(scripts/msrv.sh versions | paste -sd ' '))")
[ "$SEMVER" = 1 ] && GATE+=("semver (scripts/semver.sh)")
[ "$TSAN" = 1 ] && GATE+=("ThreadSanitizer, one round (scripts/check.sh tsan)")
[ "$LOOM" = 1 ] && GATE+=("loom (scripts/check.sh loom)")
[ "$DEPS" = 1 ] && GATE+=("supply chain (cargo deny --workspace check)")
for d in "${!FUZZ[@]}"; do GATE+=("fuzz ${d/#./giong}: every target, ${FUZZ_SECS}s each"); done
if [ ${#GATE[@]} -gt 0 ]; then
    echo "The gate also runs (not --quick):"
    printf '   %s\n' "${GATE[@]}"
fi
if [ ${#SOAK[@]} -gt 0 ]; then
    echo "Soak: on a soak path, so a release that contains it needs 2 to 4 hours per driver"
    echo "      (unless only doc comments changed): ${SOAK[*]:0:6}"
else
    echo "Soak: none needed for this change."
fi
[ ${#OTHER_WS[@]} -gt 0 ] && echo "bench/ or sim/ changed: separate workspaces; build and test them where they changed."
if [ "$SEMVER" = 1 ] && [ "$CHANGELOG_ENTRY" = 0 ]; then
    echo "Changelog: library code changed and no changelog.d/ entry was added. Add one if a"
    echo "           user would notice (changelog.d/README.md); skip it for internal changes."
fi
echo
[ "$MODE" = plan ] && exit 0

# ------------------------------------------------------------------ run
FAILED=()
PASSED=()
step() {
    local name="$1"; shift
    echo "== $name"
    local start=$SECONDS
    if "$@"; then
        PASSED+=("$name ($((SECONDS - start))s)")
    else
        FAILED+=("$name"); echo "!! $name failed"
    fi
}
need() {   # need <command to test> <what> : fail the gate with the install hint
    if ! eval "$1" > /dev/null 2>&1; then
        echo "!! $2 is not installed: scripts/preflight.sh --install"
        FAILED+=("$2 missing")
        return 1
    fi
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
        TEST_ARGS=(--workspace)
    else
        TEST_ARGS=()
        for c in "${!CRATES[@]}"; do TEST_ARGS+=(-p "$c"); done
    fi
    step "tests (io_uring)" cargo test "${TEST_ARGS[@]}" --all-features --quiet
    [ "$MODE" = gate ] && \
        step "tests (epoll)" env GIONG_DRIVER=epoll cargo test "${TEST_ARGS[@]}" --all-features --quiet
fi

if [ "$MODE" = gate ]; then
    if [ "$RUST" = 1 ] && need "scripts/msrv.sh versions | while read -r v; do cargo +\$v --version || exit 1; done" \
        "Rust $(scripts/msrv.sh versions | paste -sd ' ')"; then
        step "MSRV (scripts/msrv.sh)" scripts/msrv.sh
    fi
    if [ "$DEPS" = 1 ] && need "cargo deny --version" "cargo-deny"; then
        step "supply chain (cargo deny)" cargo deny --workspace check
    fi
    if [ "$SEMVER" = 1 ] && need "cargo semver-checks --version" "cargo-semver-checks"; then
        step "semver" scripts/semver.sh
    fi
    if { [ "$TSAN" = 1 ] || [ "$LOOM" = 1 ] || [ ${#FUZZ[@]} -gt 0 ]; } && \
        need "rustup run nightly rustc --print sysroot && test -d \"\$(rustup run nightly rustc --print sysroot)/lib/rustlib/src\"" "nightly with rust-src"; then
        [ "$TSAN" = 1 ] && step "tsan" env ROUNDS=1 scripts/check.sh tsan
        [ "$LOOM" = 1 ] && step "loom" scripts/check.sh loom
        if [ ${#FUZZ[@]} -gt 0 ] && need "cargo fuzz --version" "cargo-fuzz"; then
            target="$(rustc -vV | sed -n 's/^host: //p')"
            for d in "${!FUZZ[@]}"; do
                for t in $(cd "$d" && cargo +nightly fuzz list 2>/dev/null); do
                    step "fuzz ${d/#./giong}/$t" bash -c "cd '$d' && cargo +nightly fuzz run --target '$target' '$t' -- -max_total_time=$FUZZ_SECS"
                done
            done
        fi
    fi
fi

echo
echo "== passed: ${#PASSED[@]}, failed: ${#FAILED[@]}"
[ ${#PASSED[@]} -gt 0 ] && printf '   ok: %s\n' "${PASSED[@]}"
if [ ${#FAILED[@]} -gt 0 ]; then
    printf '   FAILED: %s\n' "${FAILED[@]}"
    exit 1
fi
[ "$MODE" = quick ] && echo "Quick mode: run scripts/preflight.sh without --quick before pushing."
echo "CI runs the fast set on the pull request, and every job, repeated, in the merge queue (docs/WORKFLOW.md)."
