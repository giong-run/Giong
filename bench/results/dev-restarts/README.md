# giong dev: restarts under load

What `giong dev` promises: a save rebuilds the service and swaps the new build in
without refusing or failing a request. This measures it.

## Result (2026-11-22)

100 saves while 16 clients loop new connections: **102,061 requests, 0 failed**, every
response from a complete build (`audit-100.out`). Save-to-serve (the file written to the
first response from the new build): median 0.752 s, p90 0.789 s, min 0.699 s, max
0.885 s.

`giong dev` counted 101 restarts: the first build wrote `Cargo.lock`, which triggered one
more build. `giong dev` no longer counts that write as a change.

## Machine

Linux 6.18 VM, 4 vCPU, io_uring driver. Debug build of the service in `service/` (2
workers, the laptop default), the giong of PR #223's branch, Rust stable.

## Method

The service answers each connection with its `VERSION` after a 20 ms sleep, so requests
are in flight at every swap. `restarts.py` runs 16 client threads, each opening a new
connection per request (so every swap meets fresh connects as well as in-flight
requests). For each of the 100 saves it rewrites `VERSION` in `src/main.rs`, waits until a
response carries the new version (that wait is the save-to-serve time), then waits 0.6 s
for the old build to retire. A request fails on any connect error, reset, timeout or
empty response.

```sh
cd bench/results/dev-restarts/service
giong dev --listen 127.0.0.1:18080 > ../giong-dev-100.out 2>&1 &
python3 ../restarts.py "$PWD" 18080 100 16 > ../audit-100.out
giong dev stop
```

`giong-dev-100.out` is `giong dev`'s own output from that run.
