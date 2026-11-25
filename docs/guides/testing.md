# Test a service

Three kinds of tests cover a giong service: plain unit tests of async code, handler tests without a socket, and recorded requests replayed against the running service. `giong test` runs the first two the way CI and production will see them.

## Async code in a plain test

There are no macros. Run a future in a test with `giong::block_on`:

```rust
#[test]
fn sums() {
    let n = giong::block_on(async { 1 + 1 });
    assert_eq!(n, 2);
}
```

## A handler without a socket

A giong-web app answers a request in memory, so a test checks the status and the coded error without binding a port:

```rust
use giong_web::{Error, Req, Request, Response, Router};

async fn get_user(req: Req<()>) -> Result<Response, Error> {
    let id: u64 = req.param("id")?;
    Ok(Response::text(format!("user {id}")))
}

#[test]
fn rejects_a_bad_id() {
    let app = Router::new().get("/users/{id}", get_user).build(());
    let req = Request::from_bytes(b"GET /users/abc HTTP/1.1\r\nHost: t\r\n\r\n").unwrap();
    let resp = giong::block_on(app.handle(req));
    assert_eq!(resp.status(), 400);
    assert_eq!(giong_web::Error::of(&resp).unwrap().code(), "GIONG-E1205");
}
```

## Run them with giong test

```sh
giong test                 # cargo test with .env, then again with GIONG_DRIVER=epoll on Linux
giong test --quick         # once
giong test -- users        # arguments after -- go to cargo test
```

- Each failing test is a finding (`GIONG-E5083`) that names the driver it failed on and what it printed.
- The second run on epoll matters: a container without io_uring (Docker's default seccomp profile, gVisor) runs on epoll.
- A run longer than `--timeout` (1800 s by default) is stopped with every process it started, naming the tests still running.
- `--nextest` uses cargo-nextest. Every flag: [`giong test`](../../giong-cli/README.md#giong-test).

## Real requests as tests

Add giong-web's `Record` middleware, use the service while `giong dev` runs, then keep the requests you want:

```sh
giong record list --status 4xx       # what was answered
giong record save last --name signup # tests/recorded/signup.json
giong record replay                  # send them again and compare, field by field
```

Ids, times and tokens the service made become placeholders, an id one request returns and a later one uses is captured, and secrets stay out of the file. A difference fails like a test (`GIONG-E5066`), so `giong record replay` belongs in CI next to `giong test`. Details: [`giong record`](../../giong-cli/README.md#giong-record) and [Safe defaults](safe-defaults.md).

## In CI

A project from `giong new` has a GitHub Actions workflow that starts the databases it needs as services and runs its tests. Keep `giong check --json` in it too: it fails on the mistakes giong knows, such as a blocking call on a worker.
