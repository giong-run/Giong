# giong dev: restarts with streamed responses in flight

[`dev-restarts`](../dev-restarts/) measured failed requests over 100 restarts with 20 ms
requests. This measures what happens to a long response that started before the save: a
streamed body (`Body::channel`) that sends one line every 100 ms.

## Result (2026-11-26)

| Streams | Service `shutdown_grace` | `giong dev --grace` | Restarts | Result |
|---|---|---|---|---|
| 3 s, 4 per restart | 10 s | 30 s (default) | 50 | **200 of 200 complete, every line from the old build** |
| 20 s, 4 per restart | 10 s | 30 s (default) | 3 | 12 of 12 cut about 10 s after the restart |
| 20 s, 4 per restart | 25 s | 30 s (default) | 3 | 12 of 12 complete on the old build |
| 20 s, 2 per restart | 60 s | 5 s | 2 | 4 of 4 cut about 5 s after the restart |

During the 50 restarts, 16 clients looping short requests on new connections made 743,169
requests with 0 failed (`audit-streams-50.out`). Save-to-serve: median 0.829 s, max 1.019 s.

So a response in flight finishes on the old build, on the old process, when it ends within
the grace: the smaller of the service's `shutdown_grace` (10 s in the `giong new` web
template) and `giong dev --grace` (30 s by default). A longer one is cut at that point. It is
cut without the end of the chunked body, so the client sees an incomplete response, not a
short complete one; a client that wants to carry on must reconnect, and its next request
goes to the new build.

## How it works

`giong dev` holds the listening sockets and hands them to every build. When the new build
accepts, the old one is told to stop accepting, then gets `SIGTERM`. With
`shutdown_on_signals(true)` the runtime stops the accept loops (`until_shutdown`) and keeps
running every task already started, the producers of streamed bodies included, for up to
`shutdown_grace`; giong-http does not cut a response in progress. `giong dev` kills an old
build still running after `--grace` (`giong-cli/src/cmd/dev.rs`, `retire` and `reap`).

## Machine

Linux 6.18 VM, 4 vCPU, io_uring driver. Debug build of the service in `service/` (2
workers, the laptop default), `giong` 1.5.1 (main at 47cdd54) built in release, Rust 1.97.

## Method

`service/` answers `GET /` with its `VERSION`, and `GET /stream?secs=N` with one line every
100 ms for N seconds, each line naming the build's `VERSION` and process id, then `done`.
`streams.py`, for each restart: opens the streams, waits 0.5 s so they are under way,
rewrites `VERSION` in `src/main.rs`, waits until `GET /` answers with the new version, then
waits for every stream to end. A stream passes when it ends with the chunked terminator,
has every line, ends with `done`, and every line carries the old version and one process id.
A short request fails on any connect error, reset, timeout or empty response. `GRACE` sets
the service's `shutdown_grace` (default 10 s).

```sh
cd bench/results/dev-restarts-streaming/service
giong dev --listen 127.0.0.1:18090 > ../giong-dev-streams.out 2>&1 &
python3 ../streams.py "$PWD" 18090 50 4 3 16 > ../audit-streams-50.out
giong dev stop
# streams longer than the grace (reset VERSION to "v0" before each run)
giong dev --listen 127.0.0.1:18090 &                       # then:
python3 ../streams.py "$PWD" 18090 3 4 20 16 > ../audit-grace10-20s.out; giong dev stop
GRACE=25 giong dev --listen 127.0.0.1:18090 &              # then:
python3 ../streams.py "$PWD" 18090 3 4 20 16 > ../audit-grace25-20s.out; giong dev stop
GRACE=60 giong dev --listen 127.0.0.1:18090 --grace 5 &    # then:
python3 ../streams.py "$PWD" 18090 2 2 20 4 > ../audit-devgrace5-20s.out; giong dev stop
```

`giong-dev-streams.out` is `giong dev`'s own output from the 50-restart run.
