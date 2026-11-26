#!/bin/sh
# Installs the giong command: curl -fsSL https://giong.run/install.sh | sh
#
# Downloads the prebuilt binary of the latest release for this machine from
# GitHub, checks its SHA-256, and puts it in ~/.giong/bin (or $GIONG_INSTALL).
# Where no binary is published (another OS or CPU), it builds the command
# with cargo instead. It never asks for root and touches nothing else, apart
# from printing the line to add to your shell's PATH.
#
#   GIONG_VERSION=1.5.0   a given version instead of the latest
#   GIONG_INSTALL=/path   where the binary goes (default ~/.giong/bin)
#   GIONG_DOWNLOAD_URL=   a mirror of the release downloads: archives are
#                         fetched from $GIONG_DOWNLOAD_URL/v<version>/

set -eu

repo="giong-run/Giong"
dir="${GIONG_INSTALL:-$HOME/.giong/bin}"

say() { printf 'giong install: %s\n' "$*" >&2; }
fail() { say "$*"; exit 1; }

fetch() {
    # fetch URL FILE
    if command -v curl > /dev/null 2>&1; then
        # An https URL stays https, redirects included: a redirect to plain
        # http (which a network in the middle could then answer) is refused.
        case "$1" in
            https://*) curl -fsSL --proto '=https' --proto-redir '=https' --tlsv1.2 --retry 3 -o "$2" "$1" ;;
            *) curl -fsSL --retry 3 -o "$2" "$1" ;;
        esac
    elif command -v wget > /dev/null 2>&1; then
        wget -q -O "$2" "$1"
    else
        fail "needs curl or wget"
    fi
}

target() {
    os=$(uname -s)
    arch=$(uname -m)
    case "$arch" in
        x86_64 | amd64) arch=x86_64 ;;
        aarch64 | arm64) arch=aarch64 ;;
        *) return 1 ;;
    esac
    case "$os" in
        # Static (musl): runs on every distribution, Alpine included.
        Linux) echo "$arch-unknown-linux-musl" ;;
        Darwin) echo "$arch-apple-darwin" ;;
        *) return 1 ;;
    esac
}

with_cargo() {
    command -v cargo > /dev/null 2>&1 \
        || fail "no prebuilt giong for this machine and no cargo to build it: install Rust from https://rustup.rs, then run this again"
    say "no prebuilt binary for this machine: building it with cargo (a minute or two)"
    if [ -n "${GIONG_VERSION:-}" ]; then
        cargo install giong-cli --locked --version "$GIONG_VERSION" --root "${dir%/bin}"
    else
        cargo install giong-cli --locked --root "${dir%/bin}"
    fi
}

latest_tag() {
    tmp_json=$(mktemp)
    if fetch "https://api.github.com/repos/$repo/releases/latest" "$tmp_json" 2> /dev/null; then
        sed -n 's/.*"tag_name": *"\([^"]*\)".*/\1/p' "$tmp_json" | head -n 1
    elif command -v curl > /dev/null 2>&1; then
        # The API allows 60 calls an hour per address; the release page's
        # redirect has no such limit: .../releases/latest -> .../releases/tag/vX.Y.Z
        curl -fsSLI --proto '=https' --proto-redir '=https' --tlsv1.2 -o /dev/null -w '%{url_effective}' \
            "https://github.com/$repo/releases/latest" \
            | sed -n 's|.*/releases/tag/\(v[^/]*\)$|\1|p'
    fi
    rm -f "$tmp_json"
}

sha256() {
    if command -v sha256sum > /dev/null 2>&1; then
        sha256sum "$1" | cut -d' ' -f1
    else
        shasum -a 256 "$1" | cut -d' ' -f1
    fi
}

main() {
    mkdir -p "$dir"
    if ! t=$(target); then
        with_cargo
        done_msg
        return
    fi
    if [ -n "${GIONG_VERSION:-}" ]; then
        tag="v${GIONG_VERSION#v}"
    else
        tag=$(latest_tag)
        [ -n "$tag" ] || fail "cannot find the latest release on GitHub (set GIONG_VERSION=x.y.z to pick one)"
    fi
    name="giong-${tag#v}-$t"
    mirror="${GIONG_DOWNLOAD_URL:-https://github.com/$repo/releases/download}"
    base="${mirror%/}/$tag"
    work=$(mktemp -d)
    trap 'rm -rf "$work"' EXIT
    if ! fetch "$base/$name.tar.gz" "$work/$name.tar.gz" 2> /dev/null; then
        # Releases before the command had binaries, or a target not built.
        with_cargo
        done_msg
        return
    fi
    fetch "$base/$name.tar.gz.sha256" "$work/sum" || fail "cannot download the checksum of $name"
    want=$(cut -d' ' -f1 < "$work/sum")
    got=$(sha256 "$work/$name.tar.gz")
    [ "$want" = "$got" ] || fail "the download of $name is damaged (SHA-256 $got, expected $want): run this again"
    tar -xzf "$work/$name.tar.gz" -C "$work"
    install -m 755 "$work/$name/giong" "$dir/giong" 2> /dev/null \
        || { cp "$work/$name/giong" "$dir/giong" && chmod 755 "$dir/giong"; }
    say "installed giong ${tag#v} in $dir"
    done_msg
}

done_msg() {
    case ":$PATH:" in
        *":$dir:"*) say "run: giong doctor" ;;
        *)
            say "add it to your PATH, for example in ~/.profile or ~/.zshrc:"
            printf '\n    export PATH="%s:$PATH"\n\n' "$dir" >&2
            say "then run: giong doctor"
            ;;
    esac
}

main "$@"
