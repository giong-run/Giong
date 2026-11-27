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
| cores | 1 core | rps | 488,350.3 | monoio 516,380.2 | 0.95 | behind |
| cores | 1 core | cpu_us_per_req | 2.0 | monoio 1.9 | 1.06 | behind |
| cores | 2 cores | rps | 1,027,919.8 | monoio 1,084,475.5 | 0.95 | behind |
| cores | 2 cores | cpu_us_per_req | 1.9 | monoio 1.8 | 1.06 | behind |
| cores | 4 cores | rps | 1,996,216.7 | monoio 1,959,341.5 | 1.02 | level |
| cores | 4 cores | cpu_us_per_req | 1.9 | tokio 2.0 | 0.97 | level |
| conns | 100 conns at 20000 rps | p99_us | 18.4 | monoio 18.2 | 1.01 | level |
| conns | 100 conns at 20000 rps | p999_us | 18.9 | monoio 19.0 | 0.99 | level |
| conns | 1000 conns at 20000 rps | p99_us | 19.0 | monoio 18.9 | 1.01 | level |
| conns | 1000 conns at 20000 rps | p999_us | 20.4 | monoio 20.0 | 1.02 | level |
| conns | 8000 conns at 20000 rps | p99_us | 23.2 | monoio 23.0 | 1.01 | level |
| conns | 8000 conns at 20000 rps | p999_us | 28.1 | tokio 27.6 | 1.02 | level |
| conns | 30000 conns at 20000 rps | p99_us | 23.7 | tokio 24.1 | 0.98 | level |
| conns | 30000 conns at 20000 rps | p999_us | 29.8 | tokio 26.2 | 1.14 | behind |

Higher is better for req/s and MB/s, lower for latency and memory.

## cores: 1 core

| runtime | req/s | p50 us | p99 us | p99.9 us | CPU us/req | peak KiB | errors |
|---|---:|---:|---:|---:|---:|---:|---:|
| giong | 488,350 (486,848 to 490,159) | 162 (161 to 162) | 168 (168 to 170) | 170 (170 to 170) | 2.0 (2.0 to 2.1) | 3,620 (3,608 to 3,620) | 0.0 |
| giong-epoll | 450,483 (441,819 to 451,866) | 142 (141 to 145) | 148 (148 to 151) | 152 (151 to 154) | 2.2 (2.2 to 2.3) | 3,220 (3,180 to 3,224) | 0.0 |
| tokio | 478,330 (478,205 to 483,621) | 136 (134 to 136) | 145 (144 to 146) | 149 (147 to 149) | 2.1 (2.1 to 2.1) | 3,952 (3,896 to 3,992) | 0.0 |
| monoio | **516,380 (510,808 to 519,930)** | **124 (123 to 125)** | **128 (128 to 129)** | **129 (129 to 130)** | **1.9 (1.9 to 2.0)** | **3,160 (3,152 to 3,228)** | 0.0 |
| glommio | 444,578 (442,909 to 473,497) | 144 (136 to 144) | 148 (141 to 149) | 150 (142 to 150) | 2.2 (2.1 to 2.3) | 14,732 (14,720 to 14,744) | 0.0 |

## cores: 2 cores

| runtime | req/s | p50 us | p99 us | p99.9 us | CPU us/req | peak KiB | errors |
|---|---:|---:|---:|---:|---:|---:|---:|
| giong | 1,027,920 (1,027,209 to 1,033,352) | 153 (152 to 154) | 162 (160 to 163) | 165 (163 to 166) | 1.9 (1.9 to 1.9) | 4,668 (4,632 to 4,680) | 0.0 |
| giong-epoll | 897,175 (896,436 to 903,452) | 143 (142 to 146) | 150 (148 to 157) | 154 (152 to 161) | 2.2 (2.2 to 2.2) | **3,848 (3,832 to 3,888)** | 0.0 |
| tokio | 927,961 (926,502 to 928,907) | 138 (137 to 138) | 203 | 214 (212 to 216) | 2.1 (2.1 to 2.1) | 4,800 (4,712 to 4,840) | 0.0 |
| monoio | **1,084,476 (1,050,108 to 1,096,715)** | **118 (116 to 121)** | **135 (122 to 136)** | **138 (124 to 139)** | **1.8 (1.8 to 1.9)** | 4,140 (4,136 to 4,196) | 0.0 |
| glommio | 915,402 (913,338 to 960,709) | 133 (123 to 133) | 161 (142 to 168) | 163 (145 to 170) | 2.2 (2.1 to 2.2) | 26,224 (26,224 to 26,236) | 0.0 |

## cores: 4 cores

| runtime | req/s | p50 us | p99 us | p99.9 us | CPU us/req | peak KiB | errors |
|---|---:|---:|---:|---:|---:|---:|---:|
| giong | **1,996,217 (1,991,192 to 2,002,580)** | 128 (128 to 129) | **138 (134 to 139)** | **143 (138 to 175)** | **1.9 (1.9 to 2.0)** | 6,788 (6,768 to 6,868) | 0.0 |
| giong-epoll | 1,962,084 (1,960,073 to 1,968,622) | **124 (120 to 135)** | 156 (153 to 178) | 159 (157 to 182) | 2.0 (2.0 to 2.0) | **5,200 (5,188 to 5,212)** | 0.0 |
| tokio | 1,679,992 (1,675,966 to 1,698,509) | 141 (140 to 144) | 333 (318 to 351) | 793 (612 to 2,335) | 2.0 (2.0 to 2.0) | 6,432 (6,116 to 6,520) | 0.0 |
| monoio | 1,959,342 (1,950,156 to 1,961,329) | 130 (128 to 132) | 151 (145 to 154) | 155 (148 to 159) | 2.0 (2.0 to 2.0) | 6,096 (6,092 to 6,096) | 0.0 |
| glommio | 1,911,230 (1,890,752 to 1,913,440) | 133 (125 to 137) | 174 (159 to 176) | 186 (166 to 189) | 2.1 (2.1 to 2.1) | 49,184 | 0.0 |

## conns: 100 conns at 20000 rps

| runtime | req/s | p50 us | p99 us | p99.9 us | CPU us/req | peak KiB | errors |
|---|---:|---:|---:|---:|---:|---:|---:|
| giong | 20,000 | **7.1** | 18.4 (18.2 to 18.4) | **18.9 (18.9 to 19.3)** | **3.0 (2.9 to 3.1)** | 3,948 (3,924 to 4,020) | 0.0 |
| giong-epoll | 20,000 | 7.2 (7.2 to 7.3) | 18.6 (18.5 to 18.6) | 19.3 | 3.2 (3.1 to 3.2) | **3,520 (3,492 to 3,532)** | 0.0 |
| tokio | 20,000 | 7.3 (7.2 to 7.3) | 18.8 (18.0 to 18.9) | 20.0 (19.5 to 20.4) | 3.2 (3.2 to 3.2) | 4,372 (4,352 to 4,392) | 0.0 |
| monoio | 20,000 | 7.2 | **18.2 (18.1 to 18.4)** | 19.0 (18.9 to 19.1) | 3.3 | 3,608 (3,608 to 3,620) | 0.0 |
| glommio | 20,000 | 8.7 | 20.2 (20.0 to 20.3) | 20.7 (20.5 to 20.8) | 6.5 (6.4 to 6.5) | 15,336 (15,288 to 15,432) | 0.0 |

## conns: 1000 conns at 20000 rps

| runtime | req/s | p50 us | p99 us | p99.9 us | CPU us/req | peak KiB | errors |
|---|---:|---:|---:|---:|---:|---:|---:|
| giong | 20,000 | **8.1** | 19.0 (19.0 to 19.1) | 20.4 | **3.1 (3.0 to 3.1)** | 12,208 (12,176 to 12,268) | 0.0 |
| giong-epoll | 20,000 | 8.2 (8.2 to 8.3) | 19.3 (19.3 to 19.4) | 20.5 (20.4 to 20.9) | 3.2 | **11,760 (11,716 to 11,776)** | 0.0 |
| tokio | 20,000 | 8.2 (8.1 to 8.3) | 19.8 (19.5 to 19.9) | 20.9 (20.7 to 21.3) | 3.2 (3.2 to 3.3) | 16,132 (16,108 to 16,192) | 0.0 |
| monoio | 20,000 | 8.1 | **18.9 (18.9 to 19.1)** | **20.0 (19.9 to 20.5)** | 3.4 (3.3 to 3.4) | 15,192 (15,192 to 15,196) | 0.0 |
| glommio | 20,000 | 10.0 | 21.3 (21.2 to 21.3) | 22.6 (22.6 to 22.8) | 7.2 | 30,836 (30,760 to 30,876) | 0.0 |

## conns: 8000 conns at 20000 rps

| runtime | req/s | p50 us | p99 us | p99.9 us | CPU us/req | peak KiB | errors |
|---|---:|---:|---:|---:|---:|---:|---:|
| giong | 20,000 | **10.1** | 23.2 (23.2 to 23.4) | 28.1 (27.6 to 28.9) | **5.0 (4.9 to 5.1)** | 75,760 (75,752 to 75,772) | 0.0 |
| giong-epoll | 20,000 | 10.6 (10.5 to 10.6) | 23.6 (23.6 to 23.7) | 28.5 (27.7 to 29.6) | 5.4 | **75,284 (75,244 to 75,328)** | 0.0 |
| tokio | 20,000 | 10.1 (10.1 to 10.2) | 23.5 | **27.6 (27.6 to 27.7)** | 5.0 | 107,668 (107,644 to 107,680) | 0.0 |
| monoio | 20,000 | 10.1 (10.1 to 10.2) | **23.0 (22.8 to 23.0)** | 27.7 (27.6 to 27.7) | 5.2 (5.1 to 5.2) | 105,324 (105,264 to 105,324) | 0.0 |
| glommio | 20,000 | 23.2 (23.1 to 23.4) | 38.8 (38.5 to 38.8) | 43.6 (43.6 to 44.4) | 19.4 (19.4 to 19.4) | 150,736 (150,736 to 150,764) | 0.0 |

## conns: 30000 conns at 20000 rps

| runtime | req/s | p50 us | p99 us | p99.9 us | CPU us/req | peak KiB | errors |
|---|---:|---:|---:|---:|---:|---:|---:|
| giong | **18,166** | **10.6** | 23.7 | 29.8 | 5.2 | 249,968 | 0.0 |
| giong-epoll | 9,359 | 11.0 | **23.5** | **25.7** | 5.6 | 129,936 | 0.0 |
| tokio | 1,565 | 17.6 | 24.1 | 26.2 | **4.8** | **33,792** | 0.0 |

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
