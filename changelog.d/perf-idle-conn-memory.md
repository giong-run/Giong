### Changed

- **giong-http** (performance): an idle connection holds less memory. HTTP/1.1: 1,318 bytes
  of heap instead of 2,062 (−36%, `bench/instr`'s `idle-http1`); HTTP/2: 3,318 instead of
  4,566 (−27%, `idle-h2`); HTTPS: 5,285 instead of 6,069 (−13%, `idle-https`). 8,000 idle HTTPS
  connections to one worker take 46.7 MiB of resident memory instead of 52.6, and over
  HTTP/2 with TLS 81.2 MiB instead of 104.4 (−23%, `bench/tls-idle.sh`). A connection's
  future keeps across an `.await` only what waiting for the next request needs: the request
  being answered, the response being written and the receipt of a body still on its way
  live in states of their own, and the rarer, larger waits (a body that is still arriving,
  a file or large body sent from where it is, the HTTP/2 preface and the last close) run in
  heap blocks the worker reuses, so that an idle connection is no longer as large as the
  largest of them. Requests take fewer instructions as well: `http1` −0.9%, `http1-client`
  −0.4%, `web` and `https` −0.2%; HTTP/2 requests are unchanged (`h2` −0.1%, `h2-streams`
  +0.02% on io_uring and −0.01% on epoll, within its noise). Allocations are unchanged.
  Method and raw data: `bench/results/idle-conn-memory/`.
