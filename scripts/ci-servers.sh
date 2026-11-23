#!/usr/bin/env bash
# The database servers of the `servers` job in .github/workflows/ci.yml, set
# up from a cache directory that actions/cache keeps between runs, so that a
# run goes to a mirror (apt, cdn.mysql.com, deb.mariadb.org, download.redis.io,
# github.com) only when the cache misses: a new server version, a new runner
# image, or a change to this script.
#
#   scripts/ci-servers.sh postgres VERSION CACHE
#   scripts/ci-servers.sh mysql    VERSION SHA256 CACHE
#   scripts/ci-servers.sh mariadb  SERIES CACHE
#   scripts/ci-servers.sh redis    VERSION SHA256 CACHE
#   scripts/ci-servers.sh valkey   VERSION SHA256 CACHE
#
# Each prints the environment the tests need (GIONG_PG_BIN, GIONG_MYSQL_BIN,
# LD_LIBRARY_PATH, GIONG_REDIS_BIN) as NAME=value lines on stdout, for
# $GITHUB_ENV. Everything fetched is checked as before: archives against their
# SHA-256, apt packages by apt, MariaDB's signing key by its fingerprint.
# Every download goes through scripts/ci-net.sh (retries, a time limit).
set -euo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
net="$here/ci-net.sh"
apt="$here/ci-apt.sh"
tmp="${RUNNER_TEMP:-/tmp}"

# Progress goes to stderr; stdout is for $GITHUB_ENV.
exec 3>&1 1>&2

postgres() {
    local v="$1" cache="$2"
    # From the PostgreSQL project's apt repository; the cluster apt creates
    # is stopped, the tests start their own.
    "$apt" install "$cache/apt-common" postgresql-common
    CI_APT_SETUP="sudo /usr/share/postgresql-common/pgdg/apt.postgresql.org.sh -y" \
        "$apt" install "$cache/apt-pg" "postgresql-$v"
    sudo systemctl stop postgresql
    echo "GIONG_PG_BIN=/usr/lib/postgresql/$v/bin" >&3
}

mysql() {
    local v="$1" sum="$2" cache="$3"
    # Oracle's generic Linux build, the minimal one (no debug binaries or test
    # suite, about 60 MB), which Oracle makes for glibc 2.17 only. mysqld
    # links libaio.so.1, which Ubuntu 24.04 renamed libaio.so.1t64. The mysql
    # client, which the tests use to set the server up, links ncurses 5, which
    # Ubuntu dropped in 24.04: it comes from Ubuntu 22.04, checked against the
    # hashes in that release's package index.
    local name="mysql-$v-linux-glibc2.17-x86_64-minimal"
    "$net" fetch "https://cdn.mysql.com/archives/mysql-${v%.*}/$name.tar.xz" "$sum" "$cache/$name.tar.xz"
    local pool=http://archive.ubuntu.com/ubuntu/pool/universe/n/ncurses
    "$net" fetch "$pool/libtinfo5_6.3-2_amd64.deb" \
        d2597b5aec92a930cf549e1b429ad892595813e72ec7814685ea146a9fb715e5 "$cache/libtinfo5_6.3-2_amd64.deb"
    "$net" fetch "$pool/libncurses5_6.3-2_amd64.deb" \
        d9b8efb37a056e01c46d93c7ef6977aa04ad2586978137d9a3954c15ad5d14c3 "$cache/libncurses5_6.3-2_amd64.deb"
    "$apt" install "$cache/apt" libaio1t64 libnuma1
    # Their only dependency, libc6, is installed: no mirror.
    sudo dpkg -i "$cache/libtinfo5_6.3-2_amd64.deb" "$cache/libncurses5_6.3-2_amd64.deb"
    sudo ln -sf /usr/lib/x86_64-linux-gnu/libaio.so.1t64 /usr/lib/x86_64-linux-gnu/libaio.so.1
    tar -xJf "$cache/$name.tar.xz" -C "$tmp"
    "$tmp/$name/bin/mysqld" --version
    "$tmp/$name/bin/mysql" --version
    echo "GIONG_MYSQL_BIN=$tmp/$name/bin" >&3
}

mariadb() {
    local series="$1" cache="$2"
    # MariaDB's own packages for this Ubuntu, from its apt repository (apt
    # checks them against MariaDB's signing key, and the key is checked
    # against its fingerprint), unpacked into a directory rather than
    # installed over the runner's MySQL; what they need from Ubuntu is
    # installed. Ubuntu has MariaDB packages of its own under the same names,
    # so each is taken at the version MariaDB's repository has, and the
    # server's version is checked.
    local debs="$cache/mariadb-debs"
    if [ ! -f "$debs/.complete" ]; then
        rm -rf "$debs"
        mkdir -p "$debs"
        "$net" retry -n 4 -t 120 -- curl -fsSL --retry 3 --retry-all-errors --connect-timeout 20 \
            -o "$tmp/mariadb-keyring.pgp" https://mariadb.org/mariadb_release_signing_key.pgp
        local fingerprint
        fingerprint=$(gpg --show-keys --with-colons "$tmp/mariadb-keyring.pgp" |
            awk -F: '$1 == "fpr" && !seen++ { print $10 }')
        if [ "$fingerprint" != 177F4010FE56CA3336300305F1656F24C74CD1D8 ]; then
            echo "MariaDB's signing key has the fingerprint $fingerprint, not the one expected"
            exit 1
        fi
        sudo install -D -m 644 "$tmp/mariadb-keyring.pgp" /etc/apt/keyrings/mariadb-keyring.pgp
        local codename
        # shellcheck disable=SC1091
        codename=$(. /etc/os-release && echo "$VERSION_CODENAME")
        echo "deb [signed-by=/etc/apt/keyrings/mariadb-keyring.pgp] https://deb.mariadb.org/$series/ubuntu $codename main" |
            sudo tee /etc/apt/sources.list.d/mariadb.list
        "$net" apt-conf
        "$net" retry -n 4 -t 300 -- sudo apt-get update -q
        local pkgs=() base p v
        for base in mariadb-server-core mariadb-server mariadb-client-core mariadb-client \
            mariadb-common libmariadb3; do
            # Older series name some packages with the version (mariadb-server-10.6).
            for p in "$base" "$base-$series"; do
                v=$(apt-cache madison "$p" 2> /dev/null | awk -F'|' '$2 ~ /maria/ && !seen++ { gsub(/ /, "", $2); print $2 }')
                if [ -n "$v" ]; then pkgs+=("$p=$v"); fi
            done
        done
        echo "${pkgs[@]}"
        (cd "$debs" && "$net" retry -n 4 -t 600 -- apt-get download "${pkgs[@]}")
        # What they need from Ubuntu, without MariaDB's and MySQL's own.
        for deb in "$debs"/*.deb; do dpkg-deb -f "$deb" Depends; done |
            tr ',' '\n' | sed -e 's/([^)]*)//g' -e 's/|.*//' -e 's/:any//' -e 's/ //g' |
            grep -v -E '^(mariadb|mysql|libmariadb|galera|default-mysql|debconf|adduser|passwd|lsb-base|init-system|perl|libdbi|libdbd|psmisc|rsync|procps|iproute2|socat|sed|\$)' |
            sort -u > "$debs/depends.txt"
        # The Ubuntu packages come from Ubuntu: MariaDB's repository is not
        # needed any more, and must not be there when ci-apt.sh runs again.
        sudo rm -f /etc/apt/sources.list.d/mariadb.list
        touch "$debs/.complete"
    fi
    # shellcheck disable=SC2046
    "$apt" install "$cache/apt-deps" $(cat "$debs/depends.txt")
    local root="$tmp/mariadb" deb
    for deb in "$debs"/*.deb; do dpkg -x "$deb" "$root"; done
    export LD_LIBRARY_PATH="$root/usr/lib/x86_64-linux-gnu"
    if ldd "$root"/usr/sbin/mariadbd "$root"/usr/bin/mariadb | grep 'not found'; then exit 1; fi
    "$root/usr/sbin/mariadbd" --version | tee /dev/stderr | grep -E -q "Ver $series\.[0-9]+-MariaDB"
    echo "LD_LIBRARY_PATH=$LD_LIBRARY_PATH" >&3
    echo "GIONG_MYSQL_BIN=$root/usr/sbin" >&3
}

# Redis and Valkey from source, with TLS, which the tests use. The built
# programs are what is kept.
redis_like() {
    local kind="$1" v="$2" sum="$3" cache="$4"
    local bin="$cache/bin"
    if [ ! -x "$bin/redis-server" ]; then
        rm -rf "$bin"
        "$apt" install "$cache/apt" libssl-dev
        local url dir
        if [ "$kind" = redis ]; then
            url="https://download.redis.io/releases/redis-$v.tar.gz"
            dir="$tmp/redis-$v"
        else
            url="https://github.com/valkey-io/valkey/archive/refs/tags/$v.tar.gz"
            dir="$tmp/valkey-$v"
        fi
        "$net" fetch "$url" "$sum" "$tmp/$kind.tar.gz"
        tar -xzf "$tmp/$kind.tar.gz" -C "$tmp"
        make -C "$dir" -j4 BUILD_TLS=yes > /dev/null
        mkdir -p "$bin"
        find "$dir/src" -maxdepth 1 -type f -perm -u+x -exec cp {} "$bin/" \;
        # Valkey answers to redis-server's name only once installed; the
        # tests look for that name.
        if [ "$kind" = valkey ]; then ln -sf valkey-server "$bin/redis-server"; fi
    fi
    "$bin/redis-server" --version
    echo "GIONG_REDIS_BIN=$bin" >&3
}

kind="${1:-}"
shift || true
case "$kind" in
    postgres) postgres "$@" ;;
    mysql) mysql "$@" ;;
    mariadb) mariadb "$@" ;;
    redis | valkey) redis_like "$kind" "$@" ;;
    *)
        echo "usage: $0 postgres|mysql|mariadb|redis|valkey ..." >&2
        exit 2
        ;;
esac
