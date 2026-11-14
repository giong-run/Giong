#!/usr/bin/env bash
# One release build of the test binaries for a whole CI run.
#
#   scripts/shared-build.sh pack <archive>     build them, then pack target/release
#   scripts/shared-build.sh unpack <archive>   put that build into a fresh checkout
#
# The release profile links every test binary with fat LTO, which takes about
# six minutes on a CI runner, and nine jobs of .github/workflows/ci.yml used to
# do it each for themselves. The `build` job there packs its target/release;
# the jobs that run the tests unpack it and then run the same cargo commands as
# before, which find the build up to date.
#
# Cargo decides that from the modification times of the source files, and a
# fresh checkout gives every file the current time, so both sides first set
# the files git tracks to one fixed time. That alone would let a build of other
# sources pass for this one, so the archive carries the git tree it was built
# from, and `unpack` refuses it unless the checkout is that tree, unchanged.
# The compiler version, flags, features and the lockfile cargo checks itself:
# where one differs, cargo rebuilds what is stale, which is slower, never wrong.
#
# Without the archive, or with one that is refused, `unpack` leaves target/
# as it was and says so; cargo then builds everything, as it did before.
set -euo pipefail
cd "$(dirname "$0")/.."

mode="${1:-}"
archive="${2:-}"
if [ -z "$archive" ] || { [ "$mode" != pack ] && [ "$mode" != unpack ]; }; then
    echo "usage: $0 pack|unpack <archive>" >&2
    exit 2
fi
case "$archive" in
    /*) ;;
    *) archive="$PWD/$archive" ;;
esac

target="${CARGO_TARGET_DIR:-target}"
# Inside target/release, so that it travels with the build.
stamp="$target/release/.shared-build-tree"

# The tree of the checked-out commit, or nothing when a tracked file differs
# from it: then the sources are not the ones any commit names.
source_tree() {
    if [ -n "$(git status --porcelain --untracked-files=no)" ]; then
        return
    fi
    git rev-parse 'HEAD^{tree}'
}

pin_mtimes() {
    git ls-files -z | xargs -0 touch -c -h -d '2020-01-01T00:00:00Z'
}

warn() {
    # An annotation on the run's page in GitHub Actions, a plain line elsewhere.
    if [ -n "${GITHUB_ACTIONS:-}" ]; then
        echo "::warning title=shared build::$1"
    else
        echo "warning: $1" >&2
    fi
}

case "$mode" in
    pack)
        tree="$(source_tree)"
        if [ -z "$tree" ]; then
            echo "tracked files differ from HEAD: a build of them cannot be shared" >&2
            exit 1
        fi
        pin_mtimes
        cargo test --release --workspace --all-features --no-run
        if [ "$(source_tree)" != "$tree" ]; then
            echo "the build changed tracked files (is Cargo.lock out of date?): it cannot be shared" >&2
            exit 1
        fi
        echo "$tree" > "$stamp"
        # --posix keeps modification times to the nanosecond, which cargo compares.
        tar --posix -C "$target" -cf - release | zstd -q -T0 -3 -o "$archive" -f
        echo "packed $(du -h "$archive" | cut -f1) for tree $tree"
        ;;
    unpack)
        if [ ! -f "$archive" ]; then
            warn "no shared build at $archive: this job builds the test binaries itself"
            exit 0
        fi
        tree="$(source_tree)"
        if [ -z "$tree" ]; then
            warn "tracked files differ from HEAD: the shared build is not used"
            exit 0
        fi
        # Next to where it goes, so that moving it there is a rename.
        mkdir -p "$target"
        scratch="$(mktemp -d "$target/shared-build.XXXXXX")"
        trap 'rm -rf "$scratch"' EXIT
        zstd -q -d -c "$archive" | tar -C "$scratch" -xf -
        built="$(cat "$scratch/release/.shared-build-tree" 2>/dev/null || true)"
        if [ "$built" != "$tree" ]; then
            warn "the shared build is of tree '${built:-unknown}', the checkout is $tree: it is not used"
            exit 0
        fi
        if [ -e "$target/release" ]; then
            warn "$target/release exists already: the shared build is not used"
            exit 0
        fi
        mv "$scratch/release" "$target/release"
        pin_mtimes
        echo "unpacked the shared build of tree $tree"
        # Cargo has the last word. If it compiles a workspace crate here, the
        # build was not shared after all: the job is still right, only slow.
        log="$scratch/cargo.log"
        cargo test --release --workspace --all-features --no-run 2>&1 | tee "$log"
        if grep -qE '^ +Compiling (giong|rest-api)' "$log"; then
            warn "cargo found the shared build out of date and compiled the workspace again"
        fi
        ;;
esac
