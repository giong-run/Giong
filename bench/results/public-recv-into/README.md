# Uploads into the program's own buffer: `recv_into` made public

`TcpStream::recv_into` and `UnixStream::recv_into` became documented API. The
benchmark lab's giong server (`bench/giong`) used to copy every request body out
of the runtime's 16 KiB pool buffers into its own `Vec`, which is what made giong
lose the lab's upload rows to Tokio (bare metal, 2026-11-27: 1 MiB uploads at
9.1 GB/s against Tokio's 17.3, 115 µs of server CPU per upload against 61). Once
a request's head is in and more than 16 KiB of its body is still to come, it now
reserves the rest and calls `recv_into` until the body is complete, the way
giong-http has received large bodies since 1.1 (`bench/results/upload-recv-into`).

## Method

Cloud VM: 4 vCPU (Xeon 2.1 GHz), Linux 6.18, rustc 1.97.0. The server runs on
CPU 0, and `bench/lab/loadgen` on CPUs 1 to 3 (3 threads, 16 connections,
closed loop, `--mix /upload --body N`, 1 s of warmup then 5 s). `main` is
`bench/giong` at 774e82a (no change to the library or the bench server since
main 2a52860), `branch` is this change, and `tokio` is `bench/tokio-raw`.
The three run in turns, three rounds; numbers are medians. CPU is the server's
user and system time per request, from `/proc`, over warmup and measurement.
`ab-up.sh` is the script, and `runs.jsonl` holds every run.

## Results

| upload | main MB/s | branch MB/s | tokio MB/s | main CPU µs/req | branch CPU µs/req | tokio CPU µs/req |
|---|---:|---:|---:|---:|---:|---:|
| 1 MiB | 4,420 | **8,032 (1.82×)** | 7,985 | 234.1 | **129.0 (−45%)** | 130.7 |
| 64 KiB | 3,005 | **3,342 (+11%)** | 3,317 | 21.7 | **19.2 (−12%)** | 19.5 |
| 1 KiB | 129 | 131 | 90 | 7.9 | 7.7 | 11.3 |

p99 at 1 MiB: 7,619 µs (main) against 3,400 µs (branch) and 4,121 µs (Tokio).
No run had an error. 1 KiB uploads do not take the new path (the whole request
fits in one receive), so that row is the same code; it is there to show nothing
else moved.

## Not measured yet

The bare-metal lab (two EPYC 4345P over 25 GbE) gets the same scenario before the
machines are returned; its rows go here and into `bench/lab`'s results when it
has run.
