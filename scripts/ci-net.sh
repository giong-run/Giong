#!/usr/bin/env bash
# Network steps of CI that survive a bad minute on a mirror.
#
#   scripts/ci-net.sh retry [-n TRIES] [-t SECS] -- CMD...
#       run CMD until it succeeds, at most TRIES times (default 5), each try
#       killed after SECS seconds (default 300), waiting 10, 20, 40... seconds
#       between tries
#   scripts/ci-net.sh fetch URL SHA256 DEST
#       download URL to DEST and check it against SHA256; a file already at
#       DEST with that hash (from a cache) is kept without going to the network
#   scripts/ci-net.sh apt-conf
#       make every apt-get of this machine retry and give up on a stalled
#       mirror after 30 seconds instead of waiting for ever
#
# Tonight's failures this answers (release 1.4.1, 2026-11-23): `curl --retry 3`
# does not retry "Connection reset by peer" (exit 56), which needs
# --retry-all-errors, and `apt-get update` hung for 30 minutes on
# archive.ubuntu.com, which a retry without a time limit never ends.
#
# Only steps that fetch something go through here. A test is never retried:
# a test that fails once is a bug (CONTRIBUTING.md), and the CI gate
# (scripts/ci-gate.py) tells the two apart by the step's name, which for these
# steps starts with "net: ".
set -euo pipefail

say() {
    if [ -n "${GITHUB_ACTIONS:-}" ]; then
        echo "::warning title=network retry::$1"
    else
        echo "ci-net: $1" >&2
    fi
}

retry() {
    local tries=5 secs=300
    while [ $# -gt 0 ]; do
        case "$1" in
            -n) tries="$2"; shift 2 ;;
            -t) secs="$2"; shift 2 ;;
            --) shift; break ;;
            *) break ;;
        esac
    done
    if [ $# -eq 0 ]; then
        echo "usage: $0 retry [-n TRIES] [-t SECS] -- CMD..." >&2
        exit 2
    fi
    local try=1 wait=10 status
    while :; do
        # --kill-after: a command that ignores SIGTERM (apt holding a lock)
        # still ends.
        if timeout --kill-after=30 "$secs" "$@"; then
            return 0
        else
            status=$?
        fi
        if [ "$try" -ge "$tries" ]; then
            say "gave up after $try tries (last exit $status): $*"
            return "$status"
        fi
        if [ "$status" -eq 124 ] || [ "$status" -eq 137 ]; then
            say "try $try of $tries took longer than ${secs}s, retrying in ${wait}s: $*"
        else
            say "try $try of $tries failed with exit $status, retrying in ${wait}s: $*"
        fi
        sleep "$wait"
        try=$((try + 1))
        wait=$((wait * 2))
    done
}

fetch() {
    if [ $# -ne 3 ]; then
        echo "usage: $0 fetch URL SHA256 DEST" >&2
        exit 2
    fi
    local url="$1" sum="$2" dest="$3"
    if [ -f "$dest" ] && echo "$sum  $dest" | sha256sum -c --status -; then
        echo "$dest: cached, $sum"
        return 0
    fi
    mkdir -p "$(dirname "$dest")"
    # curl's own retries cover a reset in the middle of a transfer; the outer
    # one covers a mirror that accepts the connection and then sends nothing.
    retry -n 4 -t 600 -- curl -fsSL --retry 3 --retry-all-errors --connect-timeout 20 \
        --speed-limit 1024 --speed-time 30 -o "$dest.part" "$url"
    if ! echo "$sum  $dest.part" | sha256sum -c -; then
        rm -f "$dest.part"
        echo "$url does not have the expected SHA-256 $sum" >&2
        exit 1
    fi
    mv "$dest.part" "$dest"
}

apt_conf() {
    printf '%s\n' \
        'Acquire::Retries "5";' \
        'Acquire::http::Timeout "30";' \
        'Acquire::https::Timeout "30";' \
        'Acquire::ftp::Timeout "30";' \
        'APT::Get::Assume-Yes "true";' \
        'Dpkg::Use-Pty "0";' |
        sudo tee /etc/apt/apt.conf.d/80-giong-ci > /dev/null
}

cmd="${1:-}"
shift || true
case "$cmd" in
    retry) retry "$@" ;;
    fetch) fetch "$@" ;;
    apt-conf) apt_conf ;;
    *)
        echo "usage: $0 retry|fetch|apt-conf ..." >&2
        exit 2
        ;;
esac
