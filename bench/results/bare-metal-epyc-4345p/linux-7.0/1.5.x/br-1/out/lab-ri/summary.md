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
| payload | answer 1 KB | payload_mb_s | 511.8 | tokio 464.5 | 1.10 | ahead |
| payload | answer 16 KB | payload_mb_s | 5,655.9 | tokio 5,308.5 | 1.07 | ahead |
| payload | answer 64 KB | payload_mb_s | 9,134.8 | tokio 9,502.8 | 0.96 | behind |
| payload | answer 1024 KB | payload_mb_s | 8,182.2 | tokio 8,828.2 | 0.93 | behind |
| payload | upload 1 KB | payload_mb_s | 507.0 | tokio 471.4 | 1.08 | ahead |
| payload | upload 64 KB | payload_mb_s | 11,266.8 | tokio 15,072.1 | 0.75 | behind |
| payload | upload 1024 KB | payload_mb_s | 17,192.2 | tokio 17,323.8 | 0.99 | level |

Higher is better for req/s and MB/s, lower for latency and memory.

## payload: answer 1 KB

| runtime | req/s | p50 us | p99 us | p99.9 us | CPU us/req | payload MB/s | peak KiB | errors |
|---|---:|---:|---:|---:|---:|---:|---:|---:|
| giong | **499,850 (483,815 to 503,220)** | **31.8 (31.7 to 33.2)** | **34.7 (34.4 to 35.5)** | 60.8 (36.5 to 62.8) | **2.0 (2.0 to 2.1)** | **512 (495 to 515)** | **3,208 (3,192 to 3,208)** | 0.0 |
| tokio | 453,654 (450,963 to 455,296) | 37.0 (37.0 to 37.2) | 41.3 (41.1 to 41.6) | **42.6 (42.4 to 42.9)** | 2.2 (2.2 to 2.2) | 464 (462 to 466) | 3,436 (3,396 to 3,476) | 0.0 |

## payload: answer 16 KB

| runtime | req/s | p50 us | p99 us | p99.9 us | CPU us/req | payload MB/s | peak KiB | errors |
|---|---:|---:|---:|---:|---:|---:|---:|---:|
| giong | **345,206 (335,406 to 345,811)** | **46.2 (46.2 to 47.5)** | **49.3 (49.0 to 50.6)** | 87.8 (77.1 to 91.4) | **2.9 (2.9 to 3.0)** | **5,656 (5,495 to 5,666)** | **3,600 (3,584 to 3,612)** | 0.0 |
| tokio | 324,007 (321,724 to 324,154) | 52.1 (52.1 to 52.6) | 57.5 (57.2 to 57.7) | **59.3 (59.0 to 59.3)** | 3.1 (3.1 to 3.1) | 5,308 (5,271 to 5,311) | 3,704 (3,628 to 3,752) | 0.0 |

## payload: answer 64 KB

| runtime | req/s | p50 us | p99 us | p99.9 us | CPU us/req | payload MB/s | peak KiB | errors |
|---|---:|---:|---:|---:|---:|---:|---:|---:|
| giong | 139,386 (139,354 to 139,912) | **113 (111 to 113)** | 220 (217 to 223) | 225 (224 to 227) | 7.2 (7.1 to 7.2) | 9,135 (9,133 to 9,169) | **5,164 (5,164 to 5,176)** | 0.0 |
| tokio | **145,001 (144,870 to 145,078)** | 117 | **126 (126 to 126)** | **128 (128 to 129)** | **6.9 (6.9 to 6.9)** | **9,503 (9,494 to 9,508)** | 5,272 (5,228 to 5,280) | 0.0 |

## payload: answer 1024 KB

| runtime | req/s | p50 us | p99 us | p99.9 us | CPU us/req | payload MB/s | peak KiB | errors |
|---|---:|---:|---:|---:|---:|---:|---:|---:|
| giong | 7,803 (7,669 to 8,128) | 2,052 (1,995 to 2,085) | **2,122 (2,093 to 2,138)** | **2,138 (2,105 to 3,858)** | 128 (123 to 130) | 8,182 (8,042 to 8,522) | **36,012 (35,996 to 36,016)** | 0.0 |
| tokio | **8,419 (8,416 to 8,681)** | **2,019 (1,978 to 2,028)** | 2,154 (2,105 to 2,154) | 2,187 (2,138 to 2,466) | **119 (115 to 119)** | **8,828 (8,824 to 9,102)** | 36,120 (36,080 to 36,124) | 0.0 |

## payload: upload 1 KB

| runtime | req/s | p50 us | p99 us | p99.9 us | CPU us/req | payload MB/s | peak KiB | errors |
|---|---:|---:|---:|---:|---:|---:|---:|---:|
| giong | **495,111 (484,512 to 504,957)** | **32.2 (31.4 to 32.9)** | **35.5 (34.9 to 47.0)** | **36.7 (36.5 to 62.3)** | **2.0 (2.0 to 2.1)** | **507 (496 to 517)** | **3,220 (3,196 to 3,224)** | 0.0 |
| tokio | 460,320 (459,108 to 461,364) | 36.5 (36.5 to 36.7) | 40.6 (40.6 to 40.8) | 41.9 | 2.2 (2.2 to 2.2) | 471 (470 to 472) | 3,372 (3,352 to 3,428) | 0.0 |

## payload: upload 64 KB

| runtime | req/s | p50 us | p99 us | p99.9 us | CPU us/req | payload MB/s | peak KiB | errors |
|---|---:|---:|---:|---:|---:|---:|---:|---:|
| giong | 171,918 (170,698 to 182,612) | 116 (90 to 125) | 138 (94 to 143) | 144 (96 to 155) | 5.8 (5.5 to 5.9) | 11,267 (11,187 to 11,968) | 5,760 (5,688 to 5,860) | 0.0 |
| tokio | **229,982 (229,409 to 230,583)** | **73.5 (73.0 to 73.5)** | **80.6** | **82.7** | **4.3 (4.3 to 4.4)** | **15,072 (15,035 to 15,112)** | **4,576 (4,568 to 4,644)** | 0.0 |

## payload: upload 1024 KB

| runtime | req/s | p50 us | p99 us | p99.9 us | CPU us/req | payload MB/s | peak KiB | errors |
|---|---:|---:|---:|---:|---:|---:|---:|---:|
| giong | 16,396 (16,260 to 16,712) | 973 (956 to 985) | **1,006 (977 to 1,225)** | **1,298 (1,026 to 1,380)** | 61.0 (59.8 to 61.5) | 17,192 (17,050 to 17,523) | 22,500 (22,484 to 23,508) | 0.0 |
| tokio | **16,521 (16,485 to 16,545)** | **928** | 1,749 (1,724 to 1,749) | 2,187 (2,122 to 2,204) | **60.5 (60.4 to 60.7)** | **17,324 (17,286 to 17,349)** | **20,108 (20,072 to 20,200)** | 0.0 |

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
