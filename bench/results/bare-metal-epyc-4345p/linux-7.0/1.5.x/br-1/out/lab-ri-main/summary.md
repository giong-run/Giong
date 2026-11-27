# Benchmark lab results

Medians of all rounds (min to max in brackets). For each point, **bold** marks the best
runtime on that metric; the last column says whether giong (io_uring) is ahead, level
(within 3%) or behind the best of the others. Raw data: `raw.csv` and `raw/`.

On a shared cloud VM the tail (p99.9) moves by several times between rounds, as the
ranges show: a single stall of the VM lands there. Treat tail rows from a cloud VM as
indicative and decide on a bare-metal run.

## Where giong stands

| scenario | point | metric | giong | best other | ratio | |
|---|---|---|---:|---|---:|---|

Higher is better for req/s and MB/s, lower for latency and memory.

## payload: answer 1 KB

| runtime | req/s | p50 us | p99 us | p99.9 us | CPU us/req | payload MB/s | peak KiB | errors |
|---|---:|---:|---:|---:|---:|---:|---:|---:|
| giong | 486,901 (480,153 to 487,968) | 32.9 (32.7 to 33.2) | 35.7 (35.2 to 36.0) | 58.8 (37.5 to 63.1) | 2.1 (2.0 to 2.1) | 499 (492 to 500) | 3,180 (3,180 to 3,208) | 0.0 |

## payload: answer 16 KB

| runtime | req/s | p50 us | p99 us | p99.9 us | CPU us/req | payload MB/s | peak KiB | errors |
|---|---:|---:|---:|---:|---:|---:|---:|---:|
| giong | 349,283 (349,137 to 350,428) | 46.0 (45.7 to 46.0) | 48.8 (48.8 to 49.0) | 51.8 (50.0 to 86.8) | 2.9 (2.9 to 2.9) | 5,723 (5,720 to 5,741) | 3,568 (3,564 to 3,572) | 0.0 |

## payload: answer 64 KB

| runtime | req/s | p50 us | p99 us | p99.9 us | CPU us/req | payload MB/s | peak KiB | errors |
|---|---:|---:|---:|---:|---:|---:|---:|---:|
| giong | 139,953 (139,588 to 140,627) | 113 (111 to 114) | 214 (119 to 221) | 223 (223 to 226) | 7.1 (7.1 to 7.2) | 9,172 (9,148 to 9,216) | 5,108 (5,104 to 5,136) | 0.0 |

## payload: answer 1024 KB

| runtime | req/s | p50 us | p99 us | p99.9 us | CPU us/req | payload MB/s | peak KiB | errors |
|---|---:|---:|---:|---:|---:|---:|---:|---:|
| giong | 7,775 (7,766 to 8,265) | 2,052 (1,962 to 2,060) | 2,105 (2,028 to 2,122) | 2,122 (2,077 to 2,154) | 129 (121 to 129) | 8,153 (8,143 to 8,666) | 36,004 (35,996 to 36,028) | 0.0 |

## payload: upload 1 KB

| runtime | req/s | p50 us | p99 us | p99.9 us | CPU us/req | payload MB/s | peak KiB | errors |
|---|---:|---:|---:|---:|---:|---:|---:|---:|
| giong | 490,925 (489,726 to 495,492) | 32.4 (32.2 to 32.6) | 34.9 (34.7 to 34.9) | 43.6 (36.2 to 48.0) | 2.0 (2.0 to 2.0) | 503 (502 to 507) | 3,212 (3,180 to 3,220) | 0.0 |

## payload: upload 64 KB

| runtime | req/s | p50 us | p99 us | p99.9 us | CPU us/req | payload MB/s | peak KiB | errors |
|---|---:|---:|---:|---:|---:|---:|---:|---:|
| giong | 153,737 (149,392 to 157,270) | 103 (101 to 106) | 199 (193 to 205) | 206 (200 to 212) | 6.5 (6.3 to 6.7) | 10,075 (9,791 to 10,307) | 7,124 (7,092 to 7,128) | 0.0 |

## payload: upload 1024 KB

| runtime | req/s | p50 us | p99 us | p99.9 us | CPU us/req | payload MB/s | peak KiB | errors |
|---|---:|---:|---:|---:|---:|---:|---:|---:|
| giong | 8,776 (8,620 to 8,999) | 1,814 (1,774 to 1,847) | 2,036 (1,823 to 2,220) | 2,351 (1,962 to 3,039) | 114 (111 to 116) | 9,202 (9,039 to 9,436) | 27,816 (27,816 to 27,888) | 0.0 |

## The matrix

| scenario | rows | where |
|---|---|---|
| cores | 1 core | cloud |
| cores | 2 cores | cloud |
| cores | 4, 8, 16+ cores | bare-metal (needs 2N CPUs: N for the server, N for the load) |
| conns | 100, 1k, 8k connections | cloud (the VM allows 20,000 file descriptors) |
| conns | 30k, 100k connections | bare-metal (needs 2N file descriptors) |
| memory | 1k, 8k idle connections | cloud |
| memory | 30k, 100k idle connections | bare-metal (needs 2N file descriptors) |
| payload | answers 1 KB, 16 KB, 64 KB, 1 MB; uploads 1 KB, 64 KB, 1 MB | cloud |
| mixed | 5% compute (~1 ms) among /json, 1 and 2 cores | cloud |
| overload | open loop past capacity, then recovery | cloud |
| loss | 0.1% and 1% packet loss | root (iptables) |
| latency | delay 1 ms +- 0.5 ms, spikes | netem (not on the cloud VM: no sch_netem) |
| soak | hours at steady load | bare-metal or a dedicated VM (long) |
