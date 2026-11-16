# Scheduler contract: worker slice and SCHED_IDLE compute lane

`bench/lanes.sh 5 8` on the tree of this branch, 4-vCPU cloud VM (Linux 6.18). Raw data and
the median/min/max table are in `cloud/`.

| scenario | what runs | p99 echo, µs (median of 5) |
|---|---|---:|
| `idle` | giong, nothing else | 249 |
| `lane-old` | compute lane as draft #118 shipped it (kernel slice, nice +10) | 228 |
| `lane` | 100 µs worker slice + `SCHED_IDLE` compute lane (new defaults) | 177 |
| `tokio` | Tokio `spawn_blocking` with the same jobs | 3,471 |
| `hog-old` | giong, kernel slice, another "program" spinning on every CPU | 955 |
| `hog` | the same with the 100 µs worker slice | 590 |
| `tokio-hog` | Tokio with the same neighbour | 3,724 |

The VM does not reproduce the bare-metal result this targets (compute lane p99 1,121 µs at
nice +10 on the EPYC 4345P with kernel 7.0, `bench/results/bare-metal-epyc-4345p/`): here
the old lane already sits at the idle level, so the `lane` row shows no gain beyond the
spread. That needs a bare-metal run. The neighbour case does show a gain: 1.6 times lower
p99 than before, 6.3 times lower than Tokio. VM spreads are wide (see min-max in
`cloud/summary.txt`).

A C microbenchmark of the same effect (a thread woken every millisecond by a timer while
four spinning threads hold the CPUs) gave p99 1,889-1,943 µs with the default slice,
77-630 µs with a 100 µs slice, and 55-68 µs when the spinning threads were `SCHED_IDLE`.
