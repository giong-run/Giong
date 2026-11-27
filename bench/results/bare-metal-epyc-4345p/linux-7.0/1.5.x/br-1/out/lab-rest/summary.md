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
| memory | 1000 idle conns | kib_per_conn | 9.2 | monoio 12.9 | 0.72 | ahead |
| memory | 8000 idle conns | kib_per_conn | 9.1 | monoio 12.9 | 0.71 | ahead |
| memory | 30000 idle conns | kib_per_conn | 9.1 | monoio 12.9 | 0.70 | ahead |
| memory | 100000 idle conns | kib_per_conn | 9.0 | monoio 12.9 | 0.70 | ahead |
| payload | answer 1 KB | payload_mb_s | 502.7 | monoio 490.6 | 1.02 | level |
| payload | answer 16 KB | payload_mb_s | 5,817.2 | monoio 5,656.3 | 1.03 | level |
| payload | answer 64 KB | payload_mb_s | 9,145.0 | tokio 9,590.6 | 0.95 | behind |
| payload | answer 1024 KB | payload_mb_s | 8,706.1 | glommio 9,080.5 | 0.96 | behind |
| payload | upload 1 KB | payload_mb_s | 499.2 | monoio 505.7 | 0.99 | level |
| payload | upload 64 KB | payload_mb_s | 10,255.6 | tokio 15,201.8 | 0.67 | behind |
| payload | upload 1024 KB | payload_mb_s | 9,094.6 | tokio 17,279.8 | 0.53 | behind |
| mixed | 1 core at 4000 rps | p99_us | 263.2 | tokio 255.5 | 1.03 | behind |
| mixed | 1 core at 4000 rps | p999_us | 504.8 | glommio 377.9 | 1.34 | behind |
| mixed | 2 cores at 8000 rps | p99_us | 265.2 | glommio 257.5 | 1.03 | level |
| mixed | 2 cores at 8000 rps | p999_us | 478.2 | glommio 375.8 | 1.27 | behind |
| overload | 1000 conns offered 600000 rps | goodput_rps | 512,186.9 | monoio 493,546.8 | 1.04 | ahead |
| overload | 1000 conns offered 600000 rps | cpu_us_per_req | 2.0 | monoio 2.0 | 0.96 | ahead |
| overload | 1000 conns offered 600000 rps | server_hwm_kib | 12,212.0 | monoio 15,272.0 | 0.80 | ahead |
| loss | 0.1% loss at 20000 rps | p99_us | 18.0 | monoio 18.2 | 0.99 | level |
| loss | 0.1% loss at 20000 rps | p999_us | 19.0 | monoio 18.9 | 1.01 | level |
| loss | 1% loss at 20000 rps | p99_us | 18.1 | tokio 18.0 | 1.01 | level |
| loss | 1% loss at 20000 rps | p999_us | 19.0 | monoio 18.9 | 1.01 | level |
| latency | netem delay 1ms 500us | p99_us | 2,875.4 | glommio 2,875.4 | 1.00 | level |
| latency | netem delay 1ms 500us | p999_us | 2,973.7 | glommio 2,973.7 | 1.00 | level |

Higher is better for req/s and MB/s, lower for latency and memory.

## memory: 1000 idle conns

| runtime | KiB/conn |
|---|---:|
| giong | 9.2 (9.2 to 9.2) |
| giong-epoll | **9.1 (9.1 to 9.1)** |
| tokio | 13.1 |
| monoio | 12.9 |
| glommio | 17.2 (17.2 to 17.2) |

## memory: 8000 idle conns

| runtime | KiB/conn |
|---|---:|
| giong | 9.1 (9.1 to 9.1) |
| giong-epoll | **9.1 (9.1 to 9.1)** |
| tokio | 13.1 |
| monoio | 12.9 (12.9 to 12.9) |
| glommio | 17.2 (17.2 to 17.2) |

## memory: 30000 idle conns

| runtime | KiB/conn |
|---|---:|
| giong | **9.1 (9.1 to 9.1)** |
| giong-epoll | 9.1 (9.0 to 9.1) |
| tokio | 13.1 (13.1 to 13.1) |
| monoio | 12.9 (12.9 to 12.9) |
| glommio | 17.2 (17.1 to 17.2) |

## memory: 100000 idle conns

| runtime | KiB/conn |
|---|---:|
| giong | **9.0 (9.0 to 9.0)** |
| giong-epoll | 9.1 (9.1 to 9.1) |
| tokio | 13.1 (13.1 to 13.1) |
| monoio | 12.9 (12.9 to 12.9) |
| glommio | 17.1 (17.1 to 17.1) |

## payload: answer 1 KB

| runtime | req/s | p50 us | p99 us | p99.9 us | CPU us/req | payload MB/s | peak KiB | errors |
|---|---:|---:|---:|---:|---:|---:|---:|---:|
| giong | **490,881 (490,797 to 503,065)** | **32.3 (31.6 to 32.6)** | **35.2 (34.7 to 61.6)** | 62.8 (36.5 to 64.4) | **2.0 (2.0 to 2.0)** | **503 (503 to 515)** | 3,208 (3,176 to 3,208) | 0.0 |
| giong-epoll | 426,643 (426,576 to 431,707) | 37.5 (37.0 to 37.5) | 40.1 (39.8 to 40.3) | 41.6 (41.1 to 41.6) | 2.3 (2.3 to 2.3) | 437 (437 to 442) | 2,760 (2,752 to 2,772) | 0.0 |
| tokio | 453,861 (451,789 to 454,244) | 37.0 (37.0 to 37.2) | 41.3 | 42.6 | 2.2 (2.2 to 2.2) | 465 (463 to 465) | 3,404 (3,384 to 3,440) | 0.0 |
| monoio | 479,140 (478,477 to 481,009) | 33.4 (33.2 to 33.4) | 35.5 (35.2 to 35.7) | **36.5 (36.2 to 36.5)** | 2.1 (2.1 to 2.1) | 491 (490 to 493) | **2,580 (2,576 to 2,584)** | 0.0 |
| glommio | 415,666 (415,541 to 424,487) | 38.5 (37.8 to 38.5) | 40.8 (39.8 to 40.8) | 41.9 (40.8 to 41.9) | 2.4 (2.4 to 2.4) | 426 (426 to 435) | 13,964 (13,876 to 13,968) | 0.0 |

## payload: answer 16 KB

| runtime | req/s | p50 us | p99 us | p99.9 us | CPU us/req | payload MB/s | peak KiB | errors |
|---|---:|---:|---:|---:|---:|---:|---:|---:|
| giong | **355,056 (349,640 to 355,671)** | **45.2 (45.2 to 45.7)** | **48.3 (48.0 to 48.5)** | **49.8 (49.5 to 87.3)** | **2.8 (2.8 to 2.9)** | **5,817 (5,728 to 5,827)** | 3,568 (3,548 to 3,624) | 0.0 |
| giong-epoll | 319,636 (311,588 to 321,270) | 50.3 (49.8 to 51.3) | 53.9 (53.1 to 54.4) | 55.2 (54.7 to 55.9) | 3.1 (3.1 to 3.2) | 5,237 (5,105 to 5,264) | 3,176 (3,152 to 3,232) | 0.0 |
| tokio | 330,262 (328,908 to 337,202) | 51.3 (50.0 to 51.3) | 56.7 (55.7 to 57.0) | 58.2 (57.2 to 58.5) | 3.0 (3.0 to 3.0) | 5,411 (5,389 to 5,525) | 3,680 (3,612 to 3,728) | 0.0 |
| monoio | 345,234 (343,855 to 346,681) | 46.5 (46.5 to 46.7) | 49.3 (49.0 to 49.8) | 50.0 (50.0 to 51.1) | 2.9 (2.9 to 2.9) | 5,656 (5,634 to 5,680) | **2,924 (2,916 to 3,004)** | 0.0 |
| glommio | 308,156 (304,059 to 316,782) | 52.1 (50.8 to 52.9) | 54.9 (53.9 to 55.4) | 55.9 (54.9 to 56.4) | 3.2 (3.2 to 3.3) | 5,049 (4,982 to 5,190) | 14,340 (14,308 to 14,340) | 0.0 |

## payload: answer 64 KB

| runtime | req/s | p50 us | p99 us | p99.9 us | CPU us/req | payload MB/s | peak KiB | errors |
|---|---:|---:|---:|---:|---:|---:|---:|---:|
| giong | 139,542 (137,432 to 142,851) | 113 (111 to 114) | 221 (118 to 226) | 225 (219 to 229) | 7.2 (7.0 to 7.3) | 9,145 (9,007 to 9,362) | 5,136 (5,124 to 5,168) | 0.0 |
| giong-epoll | 143,562 (143,272 to 144,118) | **111 (111 to 111)** | **115 (115 to 116)** | 117 (116 to 117) | 7.0 (6.9 to 7.0) | 9,408 (9,390 to 9,445) | 4,696 (4,680 to 4,712) | 0.0 |
| tokio | **146,341 (145,873 to 146,996)** | 116 (116 to 116) | 125 (124 to 125) | 127 (127 to 128) | **6.8 (6.8 to 6.9)** | **9,591 (9,560 to 9,634)** | 5,200 (5,156 to 5,224) | 0.0 |
| monoio | 142,737 (139,540 to 143,782) | 112 (111 to 114) | 116 (114 to 118) | **116 (116 to 119)** | 7.0 (6.9 to 7.2) | 9,354 (9,145 to 9,423) | **4,508 (4,504 to 4,524)** | 0.0 |
| glommio | 141,348 (141,082 to 141,570) | 113 (113 to 113) | 116 | 118 (118 to 118) | 7.1 (7.1 to 7.1) | 9,263 (9,246 to 9,278) | 15,900 (15,892 to 15,912) | 0.0 |

## payload: answer 1024 KB

| runtime | req/s | p50 us | p99 us | p99.9 us | CPU us/req | payload MB/s | peak KiB | errors |
|---|---:|---:|---:|---:|---:|---:|---:|---:|
| giong | 8,303 (7,839 to 8,539) | 1,896 (1,839 to 2,036) | 2,105 (2,077 to 2,122) | 2,138 (2,105 to 2,630) | 120 (117 to 128) | 8,706 (8,220 to 8,954) | 36,004 (35,992 to 36,056) | 0.0 |
| giong-epoll | **8,887 (8,480 to 8,977)** | **1,790 (1,774 to 1,888)** | **1,888 (1,847 to 1,946)** | 2,003 (1,995 to 2,068) | **113 (111 to 118)** | **9,319 (8,892 to 9,413)** | 35,548 (35,524 to 35,580) | 0.0 |
| tokio | 8,507 (8,492 to 9,247) | 1,978 (1,864 to 2,011) | 2,138 (2,068 to 2,269) | 2,482 (2,138 to 2,646) | 117 (108 to 118) | 8,920 (8,905 to 9,696) | 36,064 (36,056 to 36,088) | 0.0 |
| monoio | 7,915 (7,684 to 7,949) | 2,011 (2,011 to 2,085) | 2,068 (2,068 to 2,105) | 2,093 (2,085 to 2,138) | 126 (126 to 130) | 8,300 (8,057 to 8,335) | **35,336 (35,324 to 35,344)** | 0.0 |
| glommio | 8,660 (8,497 to 8,734) | 1,864 (1,823 to 1,880) | 1,905 (1,896 to 1,937) | **1,921 (1,921 to 1,946)** | 115 (114 to 118) | 9,080 (8,910 to 9,158) | 46,780 (46,760 to 46,824) | 0.0 |

## payload: upload 1 KB

| runtime | req/s | p50 us | p99 us | p99.9 us | CPU us/req | payload MB/s | peak KiB | errors |
|---|---:|---:|---:|---:|---:|---:|---:|---:|
| giong | 487,464 (482,026 to 494,106) | 32.6 (32.3 to 33.2) | 35.5 (34.7 to 35.5) | 36.7 (36.0 to 64.4) | 2.1 (2.0 to 2.1) | 499 (494 to 506) | 3,208 (3,176 to 3,284) | 0.0 |
| giong-epoll | 429,286 (428,836 to 431,210) | 37.2 (37.0 to 37.2) | 39.6 (39.6 to 39.8) | 41.1 | 2.3 (2.3 to 2.3) | 440 (439 to 442) | 2,772 (2,744 to 2,772) | 0.0 |
| tokio | 460,882 (460,148 to 461,092) | 36.5 | 40.6 (40.6 to 40.8) | 41.9 | 2.2 (2.2 to 2.2) | 472 (471 to 472) | 3,332 (3,328 to 3,384) | 0.0 |
| monoio | **493,800 (491,858 to 498,668)** | **32.3 (32.1 to 32.4)** | **34.4 (34.2 to 34.7)** | **35.5 (34.9 to 35.5)** | **2.0 (2.0 to 2.0)** | **506 (504 to 511)** | **2,564 (2,560 to 2,632)** | 0.0 |
| glommio | 423,890 (410,634 to 428,630) | 37.8 (37.2 to 38.8) | 39.8 (39.3 to 41.1) | 40.6 (40.3 to 41.9) | 2.4 (2.3 to 2.4) | 434 (420 to 439) | 13,888 (13,852 to 13,892) | 0.0 |

## payload: upload 64 KB

| runtime | req/s | p50 us | p99 us | p99.9 us | CPU us/req | payload MB/s | peak KiB | errors |
|---|---:|---:|---:|---:|---:|---:|---:|---:|
| giong | 156,488 (155,191 to 164,077) | 101 (97 to 102) | 196 (187 to 198) | 203 (193 to 204) | 6.4 (6.1 to 6.4) | 10,256 (10,171 to 10,753) | 7,132 (7,068 to 7,188) | 0.0 |
| giong-epoll | 128,170 (127,194 to 129,397) | 125 (124 to 126) | 128 (127 to 129) | 130 (129 to 131) | 7.8 (7.7 to 7.9) | 8,400 (8,336 to 8,480) | **4,124 (4,040 to 4,124)** | 0.0 |
| tokio | **231,961 (230,768 to 233,650)** | **73.0 (72.4 to 73.0)** | **80.1 (79.6 to 80.6)** | **82.2 (81.7 to 82.7)** | **4.3 (4.3 to 4.3)** | **15,202 (15,124 to 15,312)** | 4,580 (4,560 to 4,616) | 0.0 |
| monoio | 133,459 (133,436 to 133,538) | 120 | 123 (123 to 123) | 124 (124 to 125) | 7.5 (7.5 to 7.5) | 8,746 (8,745 to 8,752) | 4,392 (4,312 to 4,396) | 0.0 |
| glommio | 123,817 (122,719 to 124,546) | 129 (128 to 130) | 133 (132 to 134) | 134 (133 to 134) | 8.1 (8.0 to 8.1) | 8,114 (8,042 to 8,162) | 15,092 (15,024 to 15,116) | 0.0 |

## payload: upload 1024 KB

| runtime | req/s | p50 us | p99 us | p99.9 us | CPU us/req | payload MB/s | peak KiB | errors |
|---|---:|---:|---:|---:|---:|---:|---:|---:|
| giong | 8,673 (8,590 to 8,798) | 1,839 (1,814 to 1,864) | 2,154 (1,888 to 2,171) | 2,286 (2,122 to 2,957) | 115 (113 to 116) | 9,095 (9,008 to 9,226) | 27,772 (27,716 to 27,840) | 0.0 |
| giong-epoll | 9,374 (9,286 to 9,520) | 1,708 (1,675 to 1,724) | 1,831 (1,798 to 1,839) | **1,864 (1,847 to 2,269)** | 107 (105 to 108) | 9,830 (9,737 to 9,983) | **19,512 (19,488 to 19,604)** | 0.0 |
| tokio | **16,479 (16,475 to 16,513)** | **932 (928 to 932)** | **1,741 (1,741 to 1,749)** | 2,236 (2,204 to 2,236) | **60.6 (60.5 to 60.7)** | **17,280 (17,275 to 17,315)** | 20,088 (20,064 to 20,156) | 0.0 |
| monoio | 7,656 (7,302 to 7,992) | 2,105 (1,995 to 2,204) | 2,171 (2,085 to 2,269) | 2,220 (2,122 to 2,318) | 131 (125 to 137) | 8,028 (7,657 to 8,380) | 19,584 (19,424 to 19,684) | 0.0 |
| glommio | 10,379 (10,330 to 10,391) | 1,421 (1,405 to 1,470) | 4,276 (4,244 to 4,440) | 4,473 (4,407 to 4,506) | 96.5 (96.1 to 96.8) | 10,883 (10,832 to 10,896) | 30,460 (30,428 to 30,512) | 0.0 |

## mixed: 1 core at 4000 rps

| runtime | req/s | p50 us | p99 us | p99.9 us | CPU us/req | peak KiB | errors |
|---|---:|---:|---:|---:|---:|---:|---:|
| giong | 4,000 | **16.7 (16.4 to 16.7)** | 263 (262 to 267) | 505 (466 to 511) | **18.8 (18.4 to 18.8)** | 3,596 (3,560 to 3,612) | 0.0 |
| giong-epoll | 4,000 | 16.7 (16.7 to 16.8) | **254 (253 to 258)** | **374 (366 to 374)** | 18.8 (18.8 to 19.1) | **3,180 (3,168 to 3,272)** | 0.0 |
| tokio | 4,000 | 16.8 (16.8 to 17.0) | 256 (253 to 256) | 448 (435 to 454) | 19.1 | 3,956 (3,936 to 3,996) | 0.0 |
| monoio | 4,000 | 16.7 (16.7 to 16.8) | 269 (267 to 269) | 476 (466 to 505) | 18.8 (18.8 to 19.1) | 3,228 (3,156 to 3,232) | 0.0 |
| glommio | 4,000 | 18.2 | 262 (260 to 267) | 378 (376 to 380) | 22.2 | 14,744 (14,732 to 14,756) | 0.0 |

- giong: json p99 261.6 us, p99.9 465.9 us; json p99 267.3 us, p99.9 511.0 us; json p99 263.2 us, p99.9 504.8 us
- giong-epoll: json p99 257.5 us, p99.9 365.6 us; json p99 254.5 us, p99.9 373.8 us; json p99 253.4 us, p99.9 373.8 us
- tokio: json p99 255.5 us, p99.9 453.6 us; json p99 255.5 us, p99.9 435.2 us; json p99 253.4 us, p99.9 447.5 us
- monoio: json p99 269.3 us, p99.9 476.2 us; json p99 267.3 us, p99.9 504.8 us; json p99 269.3 us, p99.9 465.9 us
- glommio: json p99 267.3 us, p99.9 379.9 us; json p99 261.6 us, p99.9 375.8 us; json p99 259.6 us, p99.9 377.9 us

## mixed: 2 cores at 8000 rps

| runtime | req/s | p50 us | p99 us | p99.9 us | CPU us/req | peak KiB | errors |
|---|---:|---:|---:|---:|---:|---:|---:|
| giong | 8,000 (8,000 to 8,000) | 10.8 (8.5 to 16.1) | 265 (256 to 267) | 478 (472 to 526) | **17.7** | 4,644 (4,600 to 4,704) | 0.0 |
| giong-epoll | 8,000 | 10.7 (10.5 to 14.4) | **252 (247 to 256)** | **327 (310 to 335)** | 18.0 (17.8 to 18.0) | **3,828 (3,824 to 3,884)** | 0.0 |
| tokio | **8,000 (8,000 to 8,000)** | 12.2 (10.7 to 12.6) | 290 (290 to 292) | 472 (448 to 492) | 18.8 (18.8 to 18.9) | 4,772 (4,748 to 4,812) | 0.0 |
| monoio | 8,000 (8,000 to 8,000) | **10.1 (7.8 to 10.8)** | 267 (261 to 267) | 507 (488 to 534) | 18.1 (18.0 to 18.3) | 4,140 (4,084 to 4,208) | 0.0 |
| glommio | 8,000 (8,000 to 8,000) | 12.0 (11.7 to 17.7) | 258 (256 to 261) | 376 (333 to 404) | 21.2 (21.2 to 21.4) | 26,248 (26,228 to 26,256) | 0.0 |

- giong: json p99 267.3 us, p99.9 526.3 us; json p99 265.2 us, p99.9 472.1 us; json p99 255.5 us, p99.9 478.2 us
- giong-epoll: json p99 247.3 us, p99.9 334.8 us; json p99 255.5 us, p99.9 326.7 us; json p99 252.4 us, p99.9 310.3 us
- tokio: json p99 291.8 us, p99.9 492.5 us; json p99 289.8 us, p99.9 472.1 us; json p99 289.8 us, p99.9 447.5 us
- monoio: json p99 260.6 us, p99.9 488.4 us; json p99 267.3 us, p99.9 534.5 us; json p99 267.3 us, p99.9 506.9 us
- glommio: json p99 260.6 us, p99.9 404.5 us; json p99 257.5 us, p99.9 332.8 us; json p99 255.5 us, p99.9 375.8 us

## overload: 1000 conns offered 600000 rps

| runtime | answers/s | CPU us/req | peak KiB | errors |
|---|---:|---:|---:|---:|
| giong | **512,187 (509,517 to 512,768)** | **2.0 (1.9 to 2.0)** | 12,212 (12,208 to 12,320) | 0.0 |
| giong-epoll | 448,901 (447,078 to 449,954) | 2.2 (2.2 to 2.2) | **11,732 (11,728 to 11,768)** | 0.0 |
| tokio | 463,261 (462,359 to 463,747) | 2.2 (2.2 to 2.2) | 16,208 (16,140 to 16,208) | 0.0 |
| monoio | 493,547 (491,480 to 494,196) | 2.0 (2.0 to 2.0) | 15,272 (15,208 to 15,276) | 0.0 |
| glommio | 434,794 (434,784 to 438,461) | 2.3 (2.3 to 2.3) | 30,820 (30,760 to 30,872) | 0.0 |

- giong: after: p99 18.4 us, p99.9 19.0 us, errors 0; after: p99 18.5 us, p99.9 18.9 us, errors 0; after: p99 17.9 us, p99.9 19.3 us, errors 0
- giong-epoll: after: p99 18.5 us, p99.9 19.1 us, errors 0; after: p99 18.5 us, p99.9 19.3 us, errors 0; after: p99 18.5 us, p99.9 19.4 us, errors 0
- tokio: after: p99 18.9 us, p99.9 20.0 us, errors 0; after: p99 18.8 us, p99.9 20.2 us, errors 0; after: p99 18.6 us, p99.9 19.3 us, errors 0
- monoio: after: p99 18.0 us, p99.9 18.6 us, errors 0; after: p99 18.2 us, p99.9 19.0 us, errors 0; after: p99 18.1 us, p99.9 19.0 us, errors 0
- glommio: after: p99 20.3 us, p99.9 21.4 us, errors 0; after: p99 20.2 us, p99.9 20.5 us, errors 0; after: p99 20.0 us, p99.9 20.5 us, errors 0

## loss: 0.1% loss at 20000 rps

| runtime | req/s | p50 us | p99 us | p99.9 us | CPU us/req | peak KiB | errors |
|---|---:|---:|---:|---:|---:|---:|---:|
| giong | 20,000 | **7.1 (6.9 to 7.1)** | **18.0 (17.7 to 18.2)** | 19.0 (18.9 to 19.3) | **3.0 (2.9 to 3.0)** | 3,924 (3,904 to 4,016) | 0.0 |
| giong-epoll | 20,000 | 7.2 | 18.0 (17.9 to 18.5) | 19.3 (19.0 to 19.3) | 3.2 (3.2 to 3.2) | **3,520 (3,496 to 3,540)** | 0.0 |
| tokio | 20,000 | 7.2 | 18.2 (18.1 to 19.0) | 19.5 (19.4 to 19.8) | 3.2 (3.1 to 3.2) | 4,388 (4,360 to 4,396) | 0.0 |
| monoio | 20,000 | 7.2 (7.1 to 7.2) | 18.2 (18.1 to 18.2) | **18.9 (18.8 to 20.0)** | 3.3 (3.2 to 3.3) | 3,608 (3,608 to 3,696) | 0.0 |
| glommio | 20,000 | 8.7 (8.7 to 8.9) | 20.2 (20.0 to 20.2) | 20.7 (20.5 to 22.5) | 6.5 | 15,404 (15,376 to 15,408) | 0.0 |

## loss: 1% loss at 20000 rps

| runtime | req/s | p50 us | p99 us | p99.9 us | CPU us/req | peak KiB | errors |
|---|---:|---:|---:|---:|---:|---:|---:|
| giong | 20,000 | **7.0** | 18.1 (17.7 to 18.2) | 19.0 (19.0 to 19.8) | **3.0 (2.9 to 3.0)** | 3,968 (3,956 to 3,984) | 0.0 |
| giong-epoll | 20,000 | 7.2 (7.1 to 7.2) | 18.2 (18.0 to 18.6) | 19.1 (19.1 to 19.3) | 3.1 (3.1 to 3.2) | **3,536 (3,504 to 3,540)** | 0.0 |
| tokio | 20,000 | 7.2 | **18.0 (17.9 to 18.1)** | 19.6 (19.3 to 20.2) | 3.2 (3.1 to 3.2) | 4,436 (4,376 to 4,440) | 0.0 |
| monoio | 20,000 | 7.2 (7.1 to 7.2) | 18.2 (17.7 to 18.4) | **18.9 (18.6 to 19.1)** | 3.2 (3.2 to 3.4) | 3,616 (3,612 to 3,684) | 0.0 |
| glommio | 20,000 | 8.7 | 20.2 (20.0 to 20.2) | 20.5 (20.5 to 20.7) | 6.5 (6.4 to 6.5) | 15,416 (15,372 to 15,468) | 0.0 |

## latency: netem delay 1ms 500us

| runtime | req/s | p50 us | p99 us | p99.9 us | CPU us/req | peak KiB | errors |
|---|---:|---:|---:|---:|---:|---:|---:|
| giong | 19,996 (19,996 to 19,996) | 2,019 | 2,875 | 2,974 | **2.4** | 3,964 (3,928 to 4,024) | 0.0 |
| giong-epoll | 19,996 (19,996 to 19,996) | 2,019 | 2,875 | 2,974 | 2.5 (2.5 to 2.5) | **3,524 (3,492 to 3,536)** | 0.0 |
| tokio | 19,996 (19,996 to 19,996) | 2,019 | 2,875 | 2,974 | 2.5 | 4,472 (4,452 to 4,476) | 0.0 |
| monoio | **19,996 (19,996 to 19,996)** | 2,019 | 2,875 | 2,974 | 2.6 (2.5 to 2.6) | 3,616 (3,612 to 3,616) | 0.0 |
| glommio | 19,996 (19,996 to 19,996) | 2,019 | 2,875 | 2,974 | 5.7 (5.7 to 5.7) | 15,352 (15,304 to 15,424) | 0.0 |

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
