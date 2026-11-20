#!/bin/bash
# Two-machine runs after both soaks, from br-1 (10.40.96.4); br-2 is
# 10.40.96.3. Logs: /root/q2/progress.log.
set -u
. /root/.cargo/env
ulimit -n 1048576
export CARGO_NET_RETRY=10
P=10.40.96.3
L=/root/q2; mkdir -p $L
log() { echo "$(date -u +%FT%TZ) $*" >>$L/progress.log; }
REPO=https://github.com/giong-run/Giong.git
both() { bash -c "$1"; ssh root@$P "$1"; }
for f in /sys/devices/system/cpu/cpu*/cpufreq/energy_performance_preference; do echo performance >"$f"; done
ssh root@$P 'for f in /sys/devices/system/cpu/cpu*/cpufreq/energy_performance_preference; do echo performance > $f; done'

# ---- A. QUIC rr (#109): receive-buffer drops vs timer resolution for the 53 ms p99
if [[ " ${PARTS:-quic xdp gso} " == *" quic "* ]]; then
  cd /root/giong-quic/bench
  for cfg in "giong:4" "giong:1" "giong-plain:1"; do
    s=${cfg%:*}; w=${cfg#*:}; tag=$s-x$w
    ssh root@$P "netstat -su" >$L/quic-$tag-netstat-before.txt
    ssh root@$P "nohup perf stat -a -e udp:udp_fail_queue_rcv_skb -o /root/quic-$tag-perf.txt -- sleep 45 >/dev/null 2>&1 &"
    sleep 1
    SERVER=root@$P SERVER_ADDR=$P:4433 REPO=/root/giong-quic LABEL=bare-metal-drops-$tag SERVERS=$s WORKERS=$w WORKLOADS=rr \
      SERVER_CPUS=0-15 CLIENT_CPUS=0-15 ./quic-2host.sh 1 10 >$L/quic-$tag.log 2>&1
    ssh root@$P "netstat -su" >$L/quic-$tag-netstat-after.txt
    sleep 32
    scp -q root@$P:/root/quic-$tag-perf.txt $L/
    cp -r results/quic/bare-metal-drops-$tag $L/quic-$tag-results
    log "quic $tag done: $(grep -E 'udp_fail' $L/quic-$tag-perf.txt | awk '{print $1}') fail_queue_rcv_skb; p99 $(tail -n1 $L/quic-$tag-results/raw.csv | cut -d, -f7) us"
  done
fi

# ---- B. AF_XDP vs kernel sockets, giong and Tokio on the real NIC (#136)
if [[ " ${PARTS:-quic xdp gso} " == *" xdp "* ]]; then
  both "cd /root && rm -rf xdpb && git clone -q $REPO xdpb && cd xdpb && git fetch -q origin bench/xdp-ceiling && git checkout -q --detach FETCH_HEAD && . /root/.cargo/env && cd bench/xdp && cargo build --release -q" >$L/xdp-build.log 2>&1
  BIN=/root/xdpb/bench/xdp/target/release/bench-xdp
  IF=enp1s0f1np1
  # Self-recovery first: the NIC gets an ntuple rule and an XDP program.
  shutdown -r +45 "self-recovery for the XDP run" 2>>$L/xdp.err
  log "xdp: reboot timer set; $(git -C /root/xdpb log --oneline -1)"
  ethtool -k $IF | grep ntuple >$L/xdp-ntuple-before.txt
  ethtool -K $IF ntuple on 2>>$L/xdp.err
  rule=$(ethtool -N $IF flow-type udp4 dst-port 7777 action 0 2>&1 | grep -o '[0-9]*$')
  irq=$(grep -m1 "$IF-TxRx-0" /proc/interrupts | cut -d: -f1 | tr -d ' ')
  cpu=$(cat /proc/irq/$irq/smp_affinity_list | cut -d, -f1 | cut -d- -f1)
  log "xdp: ntuple rule $rule steers udp/7777 to queue 0, its irq $irq on cpu $cpu"
  O=$L/xdp; mkdir -p $O
  echo "round,mode,echoes_per_s,server_process_us_per_echo,server_cpu_softirq_us_per_echo" >$O/raw.csv
  ticks() { echo "$(awk '{print $14 + $15}' /proc/$1/stat) $(awk -v c=cpu$cpu '$1 == c {print $8}' /proc/stat)"; }
  for r in 1 2 3; do
    for m in kernel giong tokio xdp-copy xdp-zerocopy; do
      mode=${m%%-*}; xm=${m#xdp-}
      XDP_MODE=$xm $BIN server $mode $IF $cpu 2>$O/server-$m.log &
      sp=$!; sleep 2
      a=$(ticks $sp)
      line=$(ssh root@$P "taskset -c 0-7 $BIN client 10.40.96.4 8 8" 2>>$O/client.err)
      b=$(ticks $sp)
      kill -9 $sp 2>/dev/null; wait $sp 2>/dev/null; sleep 3
      e=${line%%,*}
      cpuus=$(python3 -c "
e=float('${e:-0}' or 0); a=[int(x) for x in '$a'.split()]; b=[int(x) for x in '$b'.split()]
print('-,-' if e<=0 else f'{(b[0]-a[0])/100/(e*9)*1e6:.3f},{(b[1]-a[1])/100/(e*9)*1e6:.3f}')" 2>/dev/null)
      echo "$r,$m,${e:-0},$cpuus" >>$O/raw.csv
    done
    log "xdp round $r done"
  done
  ethtool -N $IF delete $rule 2>>$L/xdp.err
  grep -q "ntuple-filters: off" $L/xdp-ntuple-before.txt && ethtool -K $IF ntuple off 2>>$L/xdp.err
  ip link show $IF | grep -q xdp && log "xdp program still attached!" || log "xdp: rule removed, no XDP program left"
  if ping -c2 -W1 $P >/dev/null; then shutdown -c; log "xdp: reboot timer cancelled"; else log "xdp: network check failed, reboot timer left on"; fi
fi

# ---- C. GSO bulk sending and GRO over the real NIC (#141)
if [[ " ${PARTS:-quic xdp gso} " == *" gso "* ]]; then
  both "cd /root && rm -rf gsob && git clone -q $REPO gsob && cd gsob && git fetch -q origin feat/udp-offload && git checkout -q --detach FETCH_HEAD && . /root/.cargo/env && cd bench/udp-echo && cargo build --release -q" >$L/gso-build.log 2>&1
  BIN=/root/gsob/bench/udp-echo/target/release/bench-udp-echo
  ssh root@$P "nohup taskset -c 2-3 $BIN sink >/root/gso-sink.log 2>&1 & echo \$! >/root/gso-sink.pid"
  sleep 1
  O=$L/gso; mkdir -p $O; echo "round,mode,datagrams_per_s,sender_cpu_ns_per_datagram" >$O/raw.csv
  for r in 1 2 3 4 5; do for m in giong giong-many giong-gso tokio kernel-gso; do
    echo "$r,$m,$(taskset -c 1 $BIN blast $m $P 3 2>>$O/blast.err)" >>$O/raw.csv; done; done
  ssh root@$P 'kill -9 $(cat /root/gso-sink.pid)'; scp -q root@$P:/root/gso-sink.log $O/
  ethtool -k enp1s0f1np1 | grep -iE "segmentation|gro" >$O/ethtool-br1.txt
  ssh root@$P "ethtool -k enp1s0f1np1 | grep -iE 'segmentation|gro'" >$O/ethtool-br2.txt
  log "gso done"
fi
log "all done"
