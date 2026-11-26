# Ship a service

From a project that passes `giong check` and `giong test` to an image running in production: measure the change, build the image, and set the few things production needs.

## Measure before you ship

```sh
giong bench --against main --path /health   # this checkout against main, round by round
giong bench http://127.0.0.1:8080/users/7 --rate auto
```

`giong bench` sends load at a fixed rate and counts each request's latency from when it was due, so a stall is charged to every request behind it. `--against` builds both versions and alternates them; `--max-regression 5` fails only when the change is slower in every round of at least three. The load generator shares the machine with the service: numbers to publish come from two machines. Every flag: [`giong bench`](../../giong-cli/README.md#giong-bench).

## Build the image

```sh
giong image --tag shop:1.0              # an OCI image, without Docker or a Dockerfile
giong image --push ghcr.io/owner/shop:1.0
```

One layer with the release binary at `/app`, CA certificates and a nonroot user, no shell. It is static when the musl target is installed (`rustup target add x86_64-unknown-linux-musl`); the same source gives the same digest. A SQLite project keeps its file in a `/data` volume (`docker run -v shop-data:/data`). `--add static,migrations` adds folders, `--load` hands the image to Docker or Podman. A project from `giong new` also has a Dockerfile, for a pipeline that wants one.

## What production needs

- **The address.** Development listens on `127.0.0.1`. In production set `LISTEN=0.0.0.0:8080` (the Dockerfile from `giong new` and the image from `giong image` do).
- **Settings from the environment.** `.env` is for development only; give production its variables (`DATABASE_URL`, secrets) from the platform. `giong env check` lists what `.env.example` expects.
- **Graceful shutdown.** Services from `giong new` shut down gracefully on `SIGTERM`. Give the platform's stop timeout a few seconds more than your slowest request.
- **Migrations.** Run `giong db migrate` against the production `DATABASE_URL` before the new version starts.

## io_uring in containers

Docker's and Kubernetes' default seccomp profiles block io_uring, so giong runs on its epoll driver there by itself, with no settings. To use io_uring, allow its three system calls with the profile in [Deployment](../DEPLOYMENT.md), which also covers the locked-memory limit on newer kernels and restarting on the same port.
