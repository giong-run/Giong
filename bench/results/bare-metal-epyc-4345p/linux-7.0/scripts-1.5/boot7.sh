#!/bin/bash
# Boot the newest installed kernel (the HWE kernel setup.sh installed) once, with grub-reboot:
# the default entry stays on the stock kernel, so a power cycle, a panic or a soft lockup
# (kernel.panic=10, softlockup_panic=1 from setup.sh) comes back on the stock kernel.
# One machine at a time: run it on br-2, wait until br-2 answers on the new kernel, then br-1.
set -euo pipefail
new=$(ls /boot/vmlinuz-* | sed 's#/boot/vmlinuz-##' | sort -V | tail -1)
if [ "$(uname -r)" = "$new" ]; then echo "already on $new"; exit 0; fi
sysctl -q kernel.panic kernel.softlockup_panic
uuid=$(grep -o "gnulinux-advanced-[0-9a-f-]*" /boot/grub/grub.cfg | head -1 | sed 's/gnulinux-advanced-//')
grub-reboot "gnulinux-advanced-$uuid>gnulinux-$new-advanced-$uuid"
echo "$(date -u +%FT%TZ) one-shot boot into $new from $(uname -r); $(grub-editenv list | tr '\n' ' ')" | tee -a /root/r15/boot.log
sync
(sleep 3; systemctl reboot) >/dev/null 2>&1 &
