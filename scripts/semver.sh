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
# rest-api is the example service, not published. A crate the baseline does
# not have (first published in this release) has nothing to be checked against.
exclude=(--exclude rest-api)
baseline_lock="$(git show "$baseline:Cargo.lock")"
for name in $(cargo metadata --no-deps --format-version 1 \
    | jq -r '.packages[] | select(.publish != []) | .name'); do
  if ! grep -qx "name = \"$name\"" <<<"$baseline_lock"; then
    echo "   $name is new since $baseline: not checked"
    exclude+=(--exclude "$name")
  fi
done
cargo semver-checks --workspace "${exclude[@]}" --baseline-rev "$baseline" --all-features
