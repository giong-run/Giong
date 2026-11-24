# A container image for the rest-api example, and a starting point for your
# own giong service. Build from the repository's root, which holds the
# workspace the example belongs to:
#
#     docker build -f examples/rest-api/Dockerfile -t rest-api .
#
# examples/rest-api/compose.yaml runs it next to PostgreSQL and Redis. For a
# service of your own, copy this file next to its Cargo.toml and change the
# package name (`-p rest-api`) and the binary's path.
#
# Nothing in the image is giong-specific: giong needs no setting to run in a
# container. With Docker's default seccomp profile it runs on its epoll
# driver and says so once on stderr (GIONG-E0001); docs/DEPLOYMENT.md shows
# how to allow io_uring, which is faster, and compose.yaml has the lines.

FROM rust:1.97-slim-bookworm AS build
WORKDIR /src
COPY . .
# The cache mounts keep crates.io downloads and compiled dependencies between
# builds; the binary is copied out of the target directory, which is a mount.
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release --locked -p rest-api \
    && cp target/release/rest-api /rest-api

# glibc and libgcc, no shell, no package manager; runs as an unprivileged user.
FROM gcr.io/distroless/cc-debian12:nonroot
COPY --from=build /rest-api /usr/local/bin/rest-api
EXPOSE 8443
# The example stops on SIGTERM, which `docker stop` and Kubernetes send:
# Builder::shutdown_on_signals(true) with a shutdown_grace of 10 s. A giong
# program that does not ask for signals ignores SIGTERM as process 1 of a
# container, and is killed 10 s later.
ENTRYPOINT ["/usr/local/bin/rest-api"]
