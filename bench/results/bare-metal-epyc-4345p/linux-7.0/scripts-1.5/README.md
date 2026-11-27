# Scripts of the 1.5.x bare-metal run

Two identical Vultr bare-metal machines on Ubuntu 24.04 with a private network between them: br-1 runs the servers, br-2 the load generators. Every script logs to `/root/r15/` on its machine and writes results to `/root/r15/out/<part>/` (raw data, conditions, a summary).

## Order

The scripts come from this branch, cloned to `/root/b/run` on both machines.

1. `setup.sh` on br-2, then on br-1. It installs packages, the HWE kernel (7.0) and rustc 1.97.0, pins the default boot entry to the running kernel, and does not reboot. Then:
   - exchange the root SSH keys it prints;
   - write `/root/r15/hosts` on both machines: `S=<br-1 private address> L=<br-2 private address>`.
2. `boot7.sh` on br-2. Wait until it answers on 7.0, then run it on br-1. It is a one-shot `grub-reboot`, so a panic, a soft lockup (`kernel.panic=10`, `softlockup_panic=1`) or a power cycle comes back on the stock kernel.
3. `tune.sh` on br-2. Once ssh answers on both addresses, run `tune.sh confirm`. Then do the same on br-1. The script sets EPP to performance, loads the kernel TLS module, unloads irdma and sets 16 combined queues on the E810 ports, under a reboot in 10 minutes that `confirm` cancels.
4. `build.sh` on both machines, at the same time. It makes the checkouts (main, v1.5.1, PR #487, PR #429, PR #180 and its base, PR #186) and puts the binaries in `/root/b/bins`.
5. `run.sh` on br-1, in tmux. These are the two-machine parts: reg, ab, rate, gzip, h3, quic. They take about 4 hours.
6. Then, at the same time:
   - on br-1: `local.sh h2 tls fs mem httpc`;
   - on br-2: `local.sh spin placement lab`. These parts turn SMT off while they run.

Never change a NIC or boot setting on both machines at once, and never without the self-recovery reboot.

## What each part measures

| part | script | question |
|---|---|---|
| reg | bench/workers.sh, bench/uring-features.sh | Has throughput per worker count (1 to 16) moved since the 0.8.0 data in `../release-0.8.0`? |
| ab | ab-net.sh | Do the 1.5.2 memory and instruction changes (and PR #487) show up over a real network? It covers HTTP/1.1, HTTPS, the TLS handshake (the #481 recheck), h2c and h2. |
| rate | rate.sh | Issue #6 again: the closed-loop maximum at 1, 2, 4 and 8 workers, open-loop p50, p99 and p99.9 at shared rates, CPU per request, and whether the server, the load generator or the link was the limit. |
| gzip | gzip.sh | #428/#429 as a whole service: requests/s, CPU per request, latency at 50% and 80% load, and retained memory, with and without `giong-web/fast-gzip`. |
| h3 | h3.sh, bench/h3 | HTTP/3 requests: giong-http's `serve_h3` against the h3 crate on quinn, both driven by a client on h3 and quinn. UDP drops are counted. |
| quic | bench/quic-2host.sh | QUIC streams (rr, bulk, many): giong's engine against quinn. |
| h2, tls, fs | bench/h2.sh, bench/tls.sh, bench/fs.sh | The one-machine benchmarks of the regression pass. |
| mem | bench/tls-idle.sh, bench/h2.sh memory | Memory per idle HTTPS and h2-over-TLS connection, and per h2 connection and stream: v1.5.1, main and PR #487. |
| httpc | bench/http-client.sh | The HTTP client (#480): v1.5.1 against main. |
| spin | PR #186's bench/spin.sh | The spin governor: is the p99 loss still there on bare metal? An opt-in eco mode ships only if it is gone. |
| placement | PR #180's bench/placement.sh | Load-aware placement against its base. |
| lab | bench/lab/lab.py | Every lab scenario: cores, conns, memory, payload, mixed, overload, loss, latency. |

`summarize.py` prints the summaries of ab, rate, gzip and h3. It reports medians over rounds with min and max. A row is marked as limited by the server (cores at least 95% busy), the load generator (at least 90% busy), the link (at least 23 Gbit/s) or neither.

## Trying the scripts on one machine

`R15_LOCAL=1 S=127.0.0.1 L=127.0.0.1 SERVER_CPUS=0 LOAD_THREADS=2` runs the two-machine scripts with br-2 as this machine, which is useful for checking that they work:

```sh
R15_LOCAL=1 S=127.0.0.1 L=127.0.0.1 SERVER_CPUS=0 LOAD_THREADS=2 \
  OUT=/tmp/smoke BINS="a=<bench-giong-http> b=<another>" WORKERS=1 CASES="http:json tls:handshake" bash ab-net.sh 2 2
```
