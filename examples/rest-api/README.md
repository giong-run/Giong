# rest-api

A complete REST API on giong: users kept in PostgreSQL, read through a Redis cache, served over HTTPS. Every piece is a giong crate ([`giong-web`](../../giong-web/README.md), [`giong-postgres`](../../giong-postgres/README.md), [`giong-redis`](../../giong-redis/README.md), [`giong-tls`](../../giong-tls/README.md)); nothing comes from the Tokio ecosystem.

```sh
openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:prime256v1 -nodes \
  -keyout key.pem -out cert.pem -days 30 -subj "/CN=localhost" -addext "subjectAltName=DNS:localhost"
DATABASE_URL=postgres://app:secret@localhost/app REDIS_URL=redis://127.0.0.1:6379 \
  TLS_CERT=cert.pem TLS_KEY=key.pem cargo run --release -p rest-api

curl -k https://localhost:8443/users -H 'Content-Type: application/json' \
  -d '{"name":"Ada Lovelace","email":"ada@example.com"}'
# {"id":1,"name":"Ada Lovelace","email":"ada@example.com","created_at":"2026-11-13T11:02:41.318402Z"}
curl -k https://localhost:8443/users/1 -i    # X-Cache: miss, then hit
```

| Route | What it does |
|---|---|
| `GET /health` | `ok` while the process runs |
| `GET /ready` | `ready` when PostgreSQL and Redis answer, `503` otherwise |
| `GET /users?limit=50&after=0` | a page of users by id (`limit` 1 to 1000), and the `after` of the next page |
| `POST /users` | creates a user: `201` with `Location`; `409` `email_taken`; `422` `invalid_name` or `invalid_email` |
| `GET /users/{id}` | one user, from Redis when it is there (`X-Cache: hit`), from PostgreSQL otherwise (`miss`, then cached) |
| `PUT /users/{id}` | replaces the name and email, and drops the cached copy |
| `DELETE /users/{id}` | `204`, and drops the cached copy |
| `GET /users/export` | every user as NDJSON, read 1,000 rows at a time and streamed |

Errors are problem+json bodies with a code (`{"status":404,"code":"user_not_found","title":"Not Found","detail":"no user has the id 9","request_id":"..."}`). A database or cache that cannot be reached gives `503` with `Retry-After`, its cause in the log only.

## Configuration

| Variable | Default | |
|---|---|---|
| `DATABASE_URL` | (required) | `postgres://user:password@host/db`; TLS to a remote host is on and verified by default |
| `REDIS_URL` | `redis://127.0.0.1:6379` | `rediss://` for TLS, `redis+unix:///path` for a socket |
| `LISTEN` | `0.0.0.0:8443` | |
| `TLS_CERT`, `TLS_KEY` | (none) | PEM files; without both, plain HTTP (behind a proxy that does TLS) |
| `WORKERS` | every core | |
| `DB_POOL`, `CACHE_POOL` | `2`, `1` | connections per worker: statements and commands pipeline over them, so a few go a long way |
| `CACHE_TTL_SECS` | `60` | |
| `ACCESS_LOG` | `stdout` | one JSON line per request, or `off` |

The table is created at startup when it is missing. SIGTERM and SIGINT stop accepting, answer the requests in progress (for up to 10 s) and exit.

## How it is built

- `src/lib.rs` is the API: the handlers and `app(state, log)`, which the test builds too. `src/main.rs` reads the environment, creates the table once, then starts one worker per core.
- Each worker has its own pools, app and listener: nothing is shared between threads and nothing is locked. The kernel spreads connections over the workers' listeners.
- Reads are cache-aside: Redis first, then PostgreSQL, then the row is cached for `CACHE_TTL_SECS`. A Redis that is down slows reads down but does not fail them. Writes delete the cached copy after the database has changed, so a read in between caches at most the old row until it expires.
- Middleware: request ids, the JSON-line access log, gzip from 1 KiB, and a 10 s timeout. Request bodies are refused past 64 KiB.
- `GET /users/export` writes its body with `Body::channel`: one page in memory at a time, whatever the table's size, and a client that reads slowly slows the reading down.

## Tests

`cargo test -p rest-api` starts a throwaway PostgreSQL cluster and Redis server (both on Unix sockets in a temporary directory), serves the API over HTTPS on a free port, and goes through every route with giong-http's client: create, conflict, validation, cache miss then hit, update and the cache dropped, pages, the export, delete, and reads that still work with the cache unreachable while `/ready` says `503`. Without PostgreSQL or Redis installed the test is skipped, unless `GIONG_PG_REQUIRED=1` or `GIONG_REDIS_REQUIRED=1`.

## Against axum

[`bench/rest-api-axum`](../../bench/rest-api-axum) is the same API on axum 0.8, tokio-postgres with deadpool-postgres, redis-rs and rustls: the same routes, SQL, validation, cache and middleware. `bench/rest-api.sh` measures both; see [Performance](#performance).

## Performance

Measured with `bench/rest-api.sh`: this example against the same API on axum 0.8.9 (Tokio 1.54, tokio-postgres 0.7.18 with deadpool-postgres 0.14.2, redis-rs 1.8.1, rustls 0.23.46 through axum-server 0.7.3, tower-http 0.6.11), on a 4-vCPU virtual machine (Xeon 2.1 GHz, Linux 6.18). Each server runs one worker on its own core and serves HTTPS (TLS 1.3, a P-256 certificate); `wrk` 4.1 runs on a second core with 50 connections; PostgreSQL 16.15 (with `fsync` off, so writes measure the servers and not the disk) and Redis 7.0.15 share the last two, with 10,000 users. `get` reads one of the first 1,000 users, from the cache; `list` reads a page of 20 from PostgreSQL; `create` inserts a user. Neither writes an access log; both add request ids, gzip and the 10 s timeout. giong keeps 2 PostgreSQL connections, axum 32, its best in a sweep of 4, 8, 16, 32 and 64. Requests per second, median of 5 interleaved rounds of 5 s, with the range; p99 is the median of the rounds' p99s. Every response was a 2xx.

| | `get` (cache) | p99 | `list` (PostgreSQL) | p99 | `create` | p99 |
|---|---|---|---|---|---|---|
| **giong** | **69.9k** (69.6–74.7k) | **1.77 ms** | **35.9k** (32.4–38.0k) | **4.47 ms** | **54.1k** (51.3–55.5k) | **4.37 ms** |
| axum | 47.2k (45.1–50.0k) | 3.23 ms | 19.5k (17.3–21.5k) | 5.39 ms | 25.8k (24.4–27.2k) | 4.85 ms |

- **1.5 times axum on cached reads, 1.8 times on pages from PostgreSQL, 2.1 times on inserts.**
- The server's core was what limited every run: 95 to 98% busy for both. On `list`, giong also kept the two database cores 190% busy out of 200, so it is close to what this PostgreSQL gives on this machine; axum kept them 168% busy.
- Resident memory, read after each measurement: 11.6 to 13.9 MB for giong, 9.1 to 9.5 MB for axum.

Not measured yet: several workers, `PUT`, `DELETE` and the export, and the access log on. Results are in `bench/results/rest-api/`.
