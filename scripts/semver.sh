#!/usr/bin/env bash
# Checks that the public API of every published crate keeps the semver promise
# against the last release, with cargo-semver-checks.
#
# The baseline is the newest tag vX.Y.Z reachable from HEAD. The release type
# comes from the versions in Cargo.toml: between releases they equal the
# baseline's, so any breaking change fails; a release pull request that bumps
# the version (a new 0.x minor, or a major) may break.
#
#   scripts/semver.sh            check against the last release
#   BASELINE=v0.7.0 scripts/semver.sh
set -euo pipefail
cd "$(dirname "$0")/.."

baseline="${BASELINE:-$(git describe --tags --abbrev=0 --match 'v[0-9]*' HEAD)}"
echo "== cargo-semver-checks against $baseline"
# rest-api is the example service, not published.
cargo semver-checks --workspace --exclude rest-api --baseline-rev "$baseline" --all-features
