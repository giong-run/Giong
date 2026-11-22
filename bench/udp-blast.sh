#!/usr/bin/env bash
# Bulk sending to one peer (bench-udp-echo blast): 1200-byte datagrams from
# one core for 3 s, four ways, 5 rounds. Needs root (network namespace, veth).
# Results: bench/results/udp-blast/.
set -u
cd "$(dirname "$0")"
(cd udp-echo && cargo build --release -q)
BIN=$PWD/udp-echo/target/release/bench-udp-echo
ip netns del xc 2>/dev/null; ip link del xs 2>/dev/null
ip netns add xc; ip link add xs type veth peer name xcl; ip link set xcl netns xc
ip addr add 10.77.0.1/24 dev xs; ip -n xc addr add 10.77.0.2/24 dev xcl
ip link set xs up; ip -n xc link set xcl up
ip netns exec xc taskset -c 2-3 $BIN sink & sk=$!
sleep 0.5
echo "round,mode,datagrams_per_s,sender_cpu_ns_per_datagram"
for r in 1 2 3 4 5; do for m in giong giong-gso tokio kernel-gso; do
  echo "$r,$m,$($BIN blast $m 10.77.0.2 3)"; done; done
kill -9 $sk; ip netns del xc; ip link del xs 2>/dev/null; true
