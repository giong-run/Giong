#!/bin/bash
# Lanes (#118) under a CPU quota, with the cgroup's throttling recorded:
# (a) 4 CPUs of quota over all 16 CPUs, (b) 4 CPUs of quota on a cpuset of 4.
. /root/.cargo/env
cd /root/giong-lanes/bench
for v in "quota4:-p CPUQuota=400%" "quota4-cpuset4:-p CPUQuota=400% -p AllowedCPUs=0-3"; do
  name=${v%%:*}; props=${v#*:}
  out=results/lanes-$name-bare-metal-epyc-4345p-b; rm -rf $out; mkdir -p $out
  systemctl reset-failed lanes2.scope 2>/dev/null
  systemd-run --quiet --scope --unit=lanes2 $props -- bash -c "
    cat /sys/fs/cgroup/system.slice/lanes2.scope/cpu.stat > $out/cpu.stat.before
    OUT=$out ./lanes.sh 5 10
    cat /sys/fs/cgroup/system.slice/lanes2.scope/cpu.stat > $out/cpu.stat.after" >/root/extras09/lanes2-$name.log 2>&1
  echo "$(date -u +%FT%TZ) lanes2 $name exit $?" >>/root/extras09/progress.log
done
