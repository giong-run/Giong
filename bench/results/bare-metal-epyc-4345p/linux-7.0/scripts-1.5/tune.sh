#!/bin/bash
# After each boot, on each machine: the conditions of the 0.9 runs.
#
#   bash tune.sh            EPP performance, kernel TLS module, then the NIC change (irdma
#                           unloaded, 16 combined queues on the E810 ports) under a
#                           self-recovery reboot in 10 minutes
#   bash tune.sh confirm    cancel the self-recovery reboot once ssh still answers over both
#                           the public and the private address
#
# Never on both machines at once: tune br-2, confirm it, then br-1.
set -uo pipefail
mkdir -p /root/r15
case ${1:-apply} in
  confirm)
    shutdown -c && echo "$(date -u +%FT%TZ) self-recovery cancelled" | tee -a /root/r15/tune.log
    ;;
  apply)
    for f in /sys/devices/system/cpu/cpu*/cpufreq/energy_performance_preference; do echo performance >"$f" 2>/dev/null; done
    modprobe tls
    shutdown -r +10 "self-recovery for the NIC change (tune.sh confirm cancels it)"
    lsmod | grep -q '^irdma' && rmmod irdma
    for i in $(ls /sys/class/net); do
      [ "$(ethtool -i "$i" 2>/dev/null | awk '/^driver/{print $2}')" = ice ] || continue
      ethtool -L "$i" combined 16 || true
    done
    {
      echo "$(date -u +%FT%TZ) $(hostname) $(uname -r): EPP $(cat /sys/devices/system/cpu/cpu0/cpufreq/energy_performance_preference 2>/dev/null), tls $(test -d /sys/module/tls && echo yes || echo no), irdma loaded: $(lsmod | grep -c '^irdma')"
      for i in $(ls /sys/class/net); do
        [ "$(ethtool -i "$i" 2>/dev/null | awk '/^driver/{print $2}')" = ice ] && echo "$i: $(ethtool -l "$i" | grep -A4 Current | grep Combined | tr -s '\t ' ' ')"
      done
      echo "self-recovery reboot pending; run 'bash tune.sh confirm' once ssh answers"
    } | tee -a /root/r15/tune.log
    ;;
esac
