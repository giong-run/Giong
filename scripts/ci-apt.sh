#!/usr/bin/env bash
# Install Ubuntu packages in CI from a cache of their .deb files, so that a
# job goes to the Ubuntu mirrors only when the runner image changes (about
# once a week) and not every time it runs. The composite action
# .github/actions/apt-install restores and saves DIR with actions/cache; this
# script fills and uses it.
#
#   scripts/ci-apt.sh install DIR PACKAGE...
#
# DIR holds the .deb files of PACKAGE... and of every dependency the runner
# image lacks, and a file .packages naming the packages they were downloaded
# for. When it does, they are installed from there with empty package lists: no
# `apt-get update`, no mirror. When it does not (a cache miss), or when that
# install fails, the packages are downloaded once, through scripts/ci-net.sh
# with retries and a time limit on every try, into DIR, and installed from
# DIR the same way, so that what is saved is what was just shown to work.
#
# CI_APT_SETUP, when set, is a command run before `apt-get update` on a miss
# (to add a repository, for example). It is not run on a hit: the .deb files
# carry everything.
#
# The cache key (in the action) holds the runner image's name and version, so
# the files always match the packages the image already has.
set -euo pipefail
here="$(cd "$(dirname "$0")" && pwd)"

if [ "${1:-}" != install ] || [ $# -lt 3 ]; then
    echo "usage: $0 install DIR PACKAGE..." >&2
    exit 2
fi
dir="$2"
shift 2
packages=("$@")
want="$(printf '%s\n' "${packages[@]}" | sort -u)"

"$here/ci-net.sh" apt-conf
mkdir -p "$dir"

note() {
    if [ -n "${GITHUB_ACTIONS:-}" ]; then
        echo "::notice title=apt cache::$1"
    else
        echo "ci-apt: $1" >&2
    fi
}

# Install from DIR only. Fails, changing nothing, when a file is missing.
offline() {
    local debs=()
    shopt -s nullglob
    debs=("$dir"/*.deb)
    shopt -u nullglob
    if [ "${#debs[@]}" -gt 0 ]; then
        # A path is what makes apt read a local file. Empty package lists
        # leave apt nothing it could download: what is not in DIR fails.
        # (--no-download refuses local files: "Pathname to install is not
        # absolute".)
        local lists
        lists="$(mktemp -d)"
        mkdir -p "$lists/partial"
        sudo apt-get install -q -o Dir::State::Lists="$lists" \
            -o Dir::Etc::SourceList=/dev/null -o Dir::Etc::SourceParts=/dev/null \
            "${debs[@]}" || return 1
    fi
    # Every package asked for is now installed (none hides behind a file
    # that was not there).
    dpkg-query -W -f '${Status}\n' "${packages[@]}" 2> /dev/null |
        grep -v -q -x 'install ok installed' && return 1
    dpkg-query -W "${packages[@]}" > /dev/null 2>&1
}

if [ -f "$dir/.packages" ] && [ "$(cat "$dir/.packages")" = "$want" ]; then
    if offline; then
        note "installed ${packages[*]} from the cache, without the mirrors"
        exit 0
    fi
    note "the cached files did not install; downloading ${packages[*]} again"
fi

# A miss: download, keep, install from what was kept.
rm -rf "$dir"
mkdir -p "$dir"
if [ -n "${CI_APT_SETUP:-}" ]; then
    "$here/ci-net.sh" retry -n 4 -t 600 -- bash -c "$CI_APT_SETUP"
fi
"$here/ci-net.sh" retry -n 4 -t 600 -- sudo apt-get update -q
sudo apt-get clean
"$here/ci-net.sh" retry -n 4 -t 900 -- \
    sudo apt-get install -q --download-only "${packages[@]}"
shopt -s nullglob
for deb in /var/cache/apt/archives/*.deb; do
    cp "$deb" "$dir/"
done
shopt -u nullglob
if offline; then
    echo "$want" > "$dir/.packages"
    note "downloaded ${packages[*]} and kept $(find "$dir" -name '*.deb' | wc -l) files for the next run"
else
    # Not kept: the next run downloads again. The install itself must still
    # happen, from the mirror.
    rm -rf "$dir"
    note "installing ${packages[*]} from the files did not work; installing from the mirror"
    "$here/ci-net.sh" retry -n 4 -t 900 -- \
        sudo apt-get install -q "${packages[@]}"
fi
