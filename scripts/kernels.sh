#!/usr/bin/env bash
# Runs the whole test suite on real older kernels, each booted in QEMU.
#
#   scripts/kernels.sh                 the default list below
#   scripts/kernels.sh 6.0.19 6.1.155  any versions built by Ubuntu's mainline
#                                      kernel archive (kernel.ubuntu.com/mainline)
#   TEST_BINARIES="target/release/deps/net-…" scripts/kernels.sh 6.0.19
#                                      only those binaries
#   TEST_ARGS="--test-threads=1" scripts/kernels.sh
#                                      arguments for every test binary
#
# The test binaries are built on the host, then packed into an initramfs with
# a static busybox and the shared libraries they need; the guest boots that
# kernel, runs every binary and powers off. No disk image, no network, no
# modules: everything the tests use (io_uring, loopback, Unix sockets, tmpfs)
# is built into Ubuntu's generic kernels.
#
# Needs qemu-system-x86_64, busybox (static), cpio and curl. Uses KVM when
# /dev/kvm is there, and is about ten times slower without it.
set -euo pipefail
cd "$(dirname "$0")/.."

VERSIONS=("$@")
if [ ${#VERSIONS[@]} -eq 0 ]; then
    # 6.0: the oldest kernel giong supports (COOP_TASKRUN, no DEFER_TASKRUN).
    # 6.1: the first with DEFER_TASKRUN, as the long-term-support series ships it.
    VERSIONS=(6.0.19 6.1.155)
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
for applet in sh mount ip poweroff cat echo uname date mkdir ulimit; do
    ln -s busybox "$root/bin/$applet"
done
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
# TEST_ARGS="--test-threads=1 name" is passed to every test binary.
printf '%s\n' "${TEST_ARGS:-}" >"$root/etc/test-args"

cat >"$root/init" <<'EOF'
#!/bin/sh
mount -t proc proc /proc
mount -t sysfs sys /sys
mount -t devtmpfs dev /dev
mount -t tmpfs tmp /tmp
ip link set lo up
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
    local base="https://kernel.ubuntu.com/mainline/v$version/amd64/"
    local deb
    deb="$(curl -fsS --retry 5 --retry-all-errors "$base" | grep -oE 'linux-image-unsigned-[^"]*-generic_[^"]*_amd64\.deb' | head -n1)"
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
    timeout "${TIMEOUT:-3600}" qemu-system-x86_64 "${accel[@]}" -smp "$CPUS" -m "$MEMORY" \
        -kernel "$kernel" -initrd "$initrd" \
        -append "console=ttyS0 quiet panic=-1 oops=panic" \
        -nographic -no-reboot </dev/null | tr -d '\r' | tee "$log" |
        grep --line-buffered -E '^GIONG-|panicked|FAILED|test result: FAILED' || true
    if ! grep -q "GIONG-KERNEL $version" "$log"; then
        echo "   the guest did not report kernel $version (see $log)"
        status=1
    elif ! grep -q '^GIONG-DONE failed=0$' "$log"; then
        echo "   failures on $version (see $log)"
        status=1
    fi
done
exit $status
