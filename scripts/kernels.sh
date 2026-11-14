#!/usr/bin/env bash
# Runs the whole test suite on real older kernels, each booted in QEMU.
#
#   scripts/kernels.sh                 the default list below
#   scripts/kernels.sh 6.0.19 6.1.155  any versions built by Ubuntu's mainline
#                                      kernel archive (kernel.ubuntu.com/mainline)
#   scripts/kernels.sh ubuntu-6.8.0-147
#                                      a kernel Ubuntu ships (linux-image-unsigned-
#                                      <version>-generic from archive.ubuntu.com)
#   TEST_BINARIES="target/release/deps/net-…" scripts/kernels.sh 6.0.19
#                                      only those binaries
#   TEST_ARGS="--test-threads=1" scripts/kernels.sh
#                                      arguments for every test binary
#   KTLS=1 scripts/kernels.sh 6.18.63  also load the kernel's tls module, so
#                                      giong-tls' kernel TLS tests run (and
#                                      must: GIONG_KTLS_REQUIRED is set)
#   SOAK_SECS=60 scripts/kernels.sh 6.18.63
#                                      then run the soak test (tests/soak.rs)
#                                      that long, on that kernel's receive path
#
# The guest also checks which driver giong chooses by itself: epoll on kernels
# older than 6.0, io_uring from 6.0 on (GIONG_TEST_EXPECT_DRIVER overrides
# that), so a kernel that quietly puts giong on epoll fails the run. With
# GIONG_TEST_EXPECT_KERNEL_WORKAROUNDS set (names separated by commas, or empty
# for none), it also checks which kernel bugs giong had to work around, and
# with GIONG_TEST_EXPECT_IO_URING_FEATURES which optional io_uring features it
# found.
#
# The test binaries are built on the host, then packed into an initramfs with
# a static busybox and the shared libraries they need; the guest boots that
# kernel, runs every binary and powers off. No disk image, no network, no
# modules: everything the tests use (io_uring, loopback, Unix sockets, tmpfs)
# is built into Ubuntu's generic kernels, except kernel TLS, a module: with
# KTLS=1 the kernel's modules package is downloaded too, and its tls and
# crypto modules go into a second initramfs archive for that kernel.
#
# Needs qemu-system-x86_64, busybox (static), cpio and curl. Uses KVM when
# /dev/kvm is there, and is about ten times slower without it.
set -euo pipefail
cd "$(dirname "$0")/.."

VERSIONS=("$@")
if [ ${#VERSIONS[@]} -eq 0 ]; then
    # 5.15: io_uring without buffer rings, so giong runs on its epoll driver.
    # 6.0: the oldest kernel giong's io_uring driver supports (COOP_TASKRUN,
    #      no DEFER_TASKRUN).
    # 6.1: the first with DEFER_TASKRUN, as the long-term-support series ships it.
    # 6.12: the first with packed buffer rings (IOU_PBUF_RING_INC).
    VERSIONS=(5.15.225 6.0.19 6.1.155 6.12.117)
fi
WORK="${CARGO_TARGET_DIR:-target}/kernels"
MEMORY="${MEMORY:-2G}"
CPUS="${CPUS:-$(nproc)}"
mkdir -p "$WORK"

busybox="$(command -v busybox)"
if ! file -L "$busybox" | grep -q 'statically linked'; then
    echo "busybox at $busybox is not statically linked (install busybox-static)" >&2
    exit 1
fi

if [ -n "${TEST_BINARIES:-}" ]; then
    # TEST_BINARIES="path ..." runs those instead (one test file, a probe).
    read -r -a BINARIES <<<"$TEST_BINARIES"
else
echo "== building the test binaries"
mapfile -t BINARIES < <(
    cargo test --release --workspace --all-features --no-run --message-format=json 2>/dev/null |
        python3 -c '
import json, sys
for line in sys.stdin:
    msg = json.loads(line)
    if msg.get("reason") == "compiler-artifact" and msg.get("executable") and msg["profile"]["test"]:
        print(msg["executable"])
'
)
fi
echo "   ${#BINARIES[@]} test binaries"

# ---------------------------------------------------------------- initramfs
root="$WORK/initramfs"
rm -rf "$root"
mkdir -p "$root"/{bin,tests,proc,sys,dev,tmp,etc}
cp "$busybox" "$root/bin/busybox"
for applet in sh mount ip poweroff cat echo uname date mkdir ulimit modprobe depmod; do
    ln -s busybox "$root/bin/$applet"
done
# The kernel loads modules it needs (the crypto behind kernel TLS) through
# this path.
mkdir -p "$root/sbin"
ln -s ../bin/busybox "$root/sbin/modprobe"
for bin in "${BINARIES[@]}"; do
    cp "$bin" "$root/tests/"
    # The shared libraries it links, at the same paths, plus the loader.
    ldd "$bin" | grep -oE '/[^ ]+' | while read -r lib; do
        mkdir -p "$root$(dirname "$lib")"
        cp -n "$lib" "$root$lib"
    done
done
printf '127.0.0.1 localhost\n::1 localhost\n' >"$root/etc/hosts"
printf 'hosts: files\n' >"$root/etc/nsswitch.conf"
# giong-tls loads the system's CA bundle; give the guest the host's.
for bundle in /etc/ssl/certs/ca-certificates.crt /etc/pki/tls/certs/ca-bundle.crt; do
    if [ -f "$bundle" ]; then
        mkdir -p "$root/etc/ssl/certs"
        cp "$bundle" "$root/etc/ssl/certs/ca-certificates.crt"
        break
    fi
done
# TEST_ARGS="--test-threads=1 name" is passed to every test binary.
printf '%s\n' "${TEST_ARGS:-}" >"$root/etc/test-args"

cat >"$root/init" <<'EOF'
#!/bin/sh
mount -t proc proc /proc
mount -t sysfs sys /sys
mount -t devtmpfs dev /dev
mount -t tmpfs tmp /tmp
ip link set lo up
# What the tests expect of this kernel, from the kernel command line.
for arg in $(cat /proc/cmdline); do
    case "$arg" in
        giong.expect_driver=*) export GIONG_TEST_EXPECT_DRIVER="${arg#*=}" ;;
        giong.expect_workarounds=*) export GIONG_TEST_EXPECT_KERNEL_WORKAROUNDS="${arg#*=}" ;;
        giong.expect_features=*) export GIONG_TEST_EXPECT_IO_URING_FEATURES="${arg#*=}" ;;
        giong.ktls=1) ktls=1 ;;
        giong.soak=*) soak="${arg#*=}" ;;
    esac
done
# A line of its own: the console's escape codes end up in front of the first one.
echo
echo "GIONG-EXPECT driver=${GIONG_TEST_EXPECT_DRIVER:-} workarounds=${GIONG_TEST_EXPECT_KERNEL_WORKAROUNDS-unchecked} features=${GIONG_TEST_EXPECT_IO_URING_FEATURES-unchecked}"
if [ -n "${ktls:-}" ]; then
    # Kernel TLS: the modules came in a second archive.
    depmod
    if modprobe tls; then
        echo "GIONG-KTLS tls module loaded"
    else
        echo "GIONG-KTLS cannot load the tls module"
    fi
    export GIONG_KTLS_REQUIRED=1
fi
# The tests open thousands of sockets; init starts with a limit of 1024.
ulimit -n 65536
export TMPDIR=/tmp HOME=/tmp RUST_BACKTRACE=1
echo "GIONG-KERNEL $(uname -r)"
failed=0
for t in /tests/*; do
    echo "GIONG-RUN $(basename "$t")"
    # shellcheck disable=SC2046
    if "$t" -q $(cat /etc/test-args); then
        echo "GIONG-PASS $(basename "$t")"
    else
        echo "GIONG-FAIL $(basename "$t")"
        failed=$((failed + 1))
    fi
done
if [ -n "${soak:-}" ]; then
    # The soak test is ignored in the run above; it gets its own here.
    for t in /tests/soak-*; do
        echo "GIONG-RUN soak for ${soak} s"
        if [ -e "$t" ] && GIONG_SOAK_SECS="$soak" "$t" -q --ignored --exact soak --nocapture; then
            echo "GIONG-PASS soak"
        else
            echo "GIONG-FAIL soak"
            failed=$((failed + 1))
        fi
    done
fi
echo "GIONG-DONE failed=$failed"
poweroff -f
EOF
chmod +x "$root/init"
initrd="$WORK/initramfs.cpio.gz"
(cd "$root" && find . | cpio -o -H newc --quiet | gzip -1) >"$initrd"

# ---------------------------------------------------------------- kernels
fetch_kernel() {
    local version="$1" dir="$WORK/linux-$1"
    if [ -f "$dir/vmlinuz" ]; then
        echo "$dir/vmlinuz"
        return
    fi
    mkdir -p "$dir"
    local base pattern deb
    case "$version" in
        ubuntu-*)
            # A kernel as Ubuntu ships it, with Ubuntu's own patches.
            base="https://archive.ubuntu.com/ubuntu/pool/main/l/linux/"
            local release="${version#ubuntu-}"
            pattern="linux-image-unsigned-${release//./\\.}-generic_[^\"]*_amd64\.deb"
            ;;
        *)
            base="https://kernel.ubuntu.com/mainline/v$version/amd64/"
            pattern='linux-image-unsigned-[^"]*-generic_[^"]*_amd64\.deb'
            ;;
    esac
    deb="$(curl -fsS --retry 5 --retry-all-errors "$base" | grep -oE "$pattern" | head -n1)"
    if [ -z "$deb" ]; then
        echo "no kernel image for $version at $base" >&2
        return 1
    fi
    # set -e does not reach into a command substitution, so fail explicitly.
    curl -fsS --retry 5 --retry-all-errors -o "$dir/image.deb" "$base$deb" || return 1
    (cd "$dir" && ar x image.deb && tar xf data.tar* ./boot) || return 1
    cp "$dir"/boot/vmlinuz-* "$dir/vmlinuz" || return 1
    rm -rf "$dir/boot" "$dir"/*.tar* "$dir/image.deb" "$dir/debian-binary"
    echo "$dir/vmlinuz"
}

# An initramfs archive with the kernel's tls module and the crypto modules
# it may need, from Ubuntu's linux-modules package for that kernel. The
# kernel unpacks it on top of the main one.
fetch_ktls_modules() {
    local version="$1" dir="$WORK/linux-$1"
    if [ -f "$dir/ktls.cpio.gz" ]; then
        echo "$dir/ktls.cpio.gz"
        return
    fi
    local base pattern deb
    case "$version" in
        ubuntu-*)
            base="https://archive.ubuntu.com/ubuntu/pool/main/l/linux/"
            local release="${version#ubuntu-}"
            pattern="linux-modules-${release//./\\.}-generic_[^\"]*_amd64\.deb"
            ;;
        *)
            base="https://kernel.ubuntu.com/mainline/v$version/amd64/"
            pattern='linux-modules-[^"]*-generic_[^"]*_amd64\.deb'
            ;;
    esac
    deb="$(curl -fsS --retry 5 --retry-all-errors "$base" | grep -oE "$pattern" | head -n1)"
    if [ -z "$deb" ]; then
        echo "no modules package for $version at $base" >&2
        return 1
    fi
    local tmp="$dir/modules"
    rm -rf "$tmp"
    mkdir -p "$tmp/root"
    curl -fsS --retry 5 --retry-all-errors -o "$tmp/modules.deb" "$base$deb" || return 1
    (cd "$tmp" && ar x modules.deb) || return 1
    # Under /lib/modules, or /usr/lib/modules in newer packages. Not every
    # kernel has all of these directories; tls.ko is checked below.
    local prefix
    for prefix in ./lib/modules ./usr/lib/modules; do
        tar xf "$tmp"/data.tar* -C "$tmp/root" --wildcards \
            "$prefix/*/kernel/net/tls" "$prefix/*/kernel/crypto" \
            "$prefix/*/kernel/arch/x86/crypto" "$prefix/*/kernel/lib/crypto" \
            "$prefix/*/modules.builtin*" "$prefix/*/modules.order" 2>/dev/null || true
    done
    if [ -d "$tmp/root/usr/lib/modules" ]; then
        mkdir -p "$tmp/root/lib"
        mv "$tmp/root/usr/lib/modules" "$tmp/root/lib/modules"
        rm -rf "$tmp/root/usr"
    fi
    # busybox's modprobe reads neither compressed modules nor kmod's indexes.
    find "$tmp/root" \( -name '*.ko.zst' -exec zstd -q -d --rm {} \; \) \
        -o \( -name '*.ko.xz' -exec xz -d {} \; \)
    if ! find "$tmp/root" -name 'tls.ko' | grep -q .; then
        echo "no tls.ko in $deb" >&2
        return 1
    fi
    (cd "$tmp/root" && find . | cpio -o -H newc --quiet | gzip -1) >"$dir/ktls.cpio.gz"
    rm -rf "$tmp"
    echo "$dir/ktls.cpio.gz"
}

accel=(-accel tcg)
if [ -w /dev/kvm ]; then
    accel=(-accel kvm -cpu host)
else
    accel=(-accel tcg,thread=multi -cpu max)
    echo "== no /dev/kvm: emulating, which is slow"
fi

status=0
for version in "${VERSIONS[@]}"; do
    echo "== Linux $version"
    if ! kernel="$(fetch_kernel "$version")"; then
        echo "   could not download kernel $version"
        status=1
        continue
    fi
    log="$WORK/linux-$version.log"
    release="${version#ubuntu-}"
    expect="${GIONG_TEST_EXPECT_DRIVER:-}"
    if [ -z "$expect" ]; then
        case "$release" in
            [0-5].*) expect=epoll ;;
            *) expect=io_uring ;;
        esac
    fi
    cmdline="console=ttyS0 quiet panic=-1 oops=panic giong.expect_driver=$expect"
    if [ -n "${GIONG_TEST_EXPECT_KERNEL_WORKAROUNDS+set}" ]; then
        cmdline+=" giong.expect_workarounds=$GIONG_TEST_EXPECT_KERNEL_WORKAROUNDS"
    fi
    if [ -n "${GIONG_TEST_EXPECT_IO_URING_FEATURES+set}" ]; then
        cmdline+=" giong.expect_features=$GIONG_TEST_EXPECT_IO_URING_FEATURES"
    fi
    if [ -n "${SOAK_SECS:-}" ]; then
        cmdline+=" giong.soak=$SOAK_SECS"
    fi
    guest_initrd="$initrd"
    if [ "${KTLS:-0}" = 1 ]; then
        if ! modules="$(fetch_ktls_modules "$version")"; then
            echo "   could not download the modules of kernel $version"
            status=1
            continue
        fi
        guest_initrd="$WORK/linux-$version/initramfs-ktls.cpio.gz"
        cat "$initrd" "$modules" >"$guest_initrd"
        cmdline+=" giong.ktls=1"
    fi
    timeout "${TIMEOUT:-3600}" qemu-system-x86_64 "${accel[@]}" -smp "$CPUS" -m "$MEMORY" \
        -kernel "$kernel" -initrd "$guest_initrd" \
        -append "$cmdline" \
        -nographic -no-reboot </dev/null | tr -d '\r' | tee "$log" |
        grep --line-buffered -E '^GIONG-|panicked|FAILED|test result: FAILED' || true
    if [ "${KTLS:-0}" = 1 ] && ! grep -q '^GIONG-KTLS tls module loaded' "$log"; then
        echo "   the guest could not load the tls module (see $log)"
        status=1
    fi
    if ! grep -q "GIONG-KERNEL $release" "$log"; then
        echo "   the guest did not report kernel $version (see $log)"
        status=1
    elif ! grep -q '^GIONG-DONE failed=0$' "$log"; then
        echo "   failures on $version (see $log)"
        status=1
    fi
done
exit $status
