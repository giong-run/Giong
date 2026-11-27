#!/usr/bin/env bash
# Writes the software bill of materials of the giong command for one target,
# the file release.yml attaches to a GitHub Release next to the archive and
# attests (actions/attest):
#
#   scripts/sbom-cli.sh <target> <version> <out-dir>
#
# writes <out-dir>/giong-<version>-<target>.cdx.json, a CycloneDX 1.5 JSON
# document listing every crate compiled into giong for that target (with
# versions, licenses and package URLs), and prints its path. Needs
# cargo-cyclonedx and jq. Used by release.yml and by install.yml.
set -euo pipefail

target=$1 version=$2 out=$3
mkdir -p "$out"
# Under SOURCE_DATE_EPOCH cargo-cyclonedx leaves the document's serialNumber
# out, and actions/attest refuses a CycloneDX document without one.
unset SOURCE_DATE_EPOCH
# cargo-cyclonedx writes <bin>_bin.cdx.json next to the manifest of every
# workspace member that has a binary (the examples too): keep giong's, drop
# the others.
cargo cyclonedx --manifest-path giong-cli/Cargo.toml --format json --spec-version 1.5 \
    --describe binaries --target "$target" --quiet
sbom="$out/giong-$version-$target.cdx.json"
mv giong-cli/giong_bin.cdx.json "$sbom"
find . -name '*_bin.cdx.json' -not -path './target/*' -delete
# What was built: the SBOM names this target and the version being released,
# and has what actions/attest needs to take it as CycloneDX.
fail() { echo "$sbom $1" >&2; rm -f "$sbom"; exit 1; }
jq -e '.bomFormat == "CycloneDX" and (.serialNumber | type == "string") and (.specVersion | type == "string")' \
    "$sbom" > /dev/null || fail "is not a CycloneDX document actions/attest takes"
[ "$(jq -r '.metadata.component.version' "$sbom")" = "$version" ] || fail "does not describe giong $version"
jq -e --arg t "$target" \
    '.metadata.properties | any(.name == "cdx:rustc:sbom:target:triple" and .value == $t)' \
    "$sbom" > /dev/null || fail "does not describe $target"
echo "$sbom"
