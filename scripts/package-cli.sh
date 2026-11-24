#!/usr/bin/env bash
# Packages a built giong command the way giong.run/install.sh and install.ps1
# expect it on a GitHub Release:
#
#   scripts/package-cli.sh <target> <version> <out-dir>
#
# reads target/<target>/release/giong[.exe] and writes, in <out-dir>,
# giong-<version>-<target>.tar.gz (.zip for Windows) holding
# giong-<version>-<target>/giong[.exe], and the archive's .sha256
# ("<hex>  <file>"). Prints the archive's path. Used by release.yml and by
# install.yml, which installs the archive with the scripts.
set -euo pipefail

target=$1 version=$2 out=$3
name="giong-$version-$target"
mkdir -p "$out"
stage=$(mktemp -d)
trap 'rm -rf "$stage"' EXIT
mkdir "$stage/$name"
cp LICENSE-MIT LICENSE-APACHE giong-cli/README.md "$stage/$name/"
case "$target" in
    *windows*)
        cp "target/$target/release/giong.exe" "$stage/$name/"
        archive="$name.zip"
        (cd "$stage" && 7z a -tzip "$archive" "$name" > /dev/null)
        ;;
    *)
        cp "target/$target/release/giong" "$stage/$name/"
        archive="$name.tar.gz"
        tar -C "$stage" -czf "$stage/$archive" "$name"
        ;;
esac
mv "$stage/$archive" "$out/"
cd "$out"
if command -v sha256sum > /dev/null; then
    sum=$(sha256sum "$archive" | cut -d' ' -f1)
else
    sum=$(shasum -a 256 "$archive" | cut -d' ' -f1)
fi
echo "$sum  $archive" > "$archive.sha256"
echo "$out/$archive"
