# Deploying giong

giong is fastest on io_uring, which needs Linux 6.0 or newer (6.1 or newer recommended)
and the three io_uring system calls: `io_uring_setup`, `io_uring_enter` and
`io_uring_register`. Many container platforms block them by default.

Where io_uring cannot be used, giong runs on its **epoll driver** instead, without being
told: in a Docker container with the default seccomp profile, in Kubernetes with the
`RuntimeDefault` profile, under gVisor, with the `kernel.io_uring_disabled` sysctl set,
on a kernel older than 6.0, or on Linux 6.14 to 7.2 when the
[locked-memory limit](#locked-memory-limit) is used up. Every API works the same way on
both drivers; epoll makes more system calls per request, and file operations run on a
pool of blocking threads.
When giong falls back by itself it says so once on stderr, with the reason (the same
`GIONG-E…` error a runtime that insists on io_uring would get). `giong::driver_choice()`
tells which driver a worker got, who chose it, and why io_uring is not used; log it at
startup:

```rust
giong::block_on(async {
    eprintln!("giong driver: {}", giong::driver_choice());
    // io_uring (chosen automatically)
});
```

`giong-mcp`'s `overview` shows the same for each worker. The
[README](../README.md#two-drivers-io_uring-and-epoll) has the measured difference.

So a container needs no special settings to run giong. Allow io_uring, as this page shows,
when you want its speed:

- `Builder::driver(DriverKind::IoUring)`, or `GIONG_DRIVER=io_uring` in the environment,
  makes the runtime fail to start where io_uring is blocked, instead of running on epoll.
  The error begins with `giong needs io_uring, which is not available here`
  ([GIONG-E0001](errors/E0001.md)).
- `Builder::driver(DriverKind::Epoll)`, or `GIONG_DRIVER=epoll`, uses epoll even where
  io_uring is available.

## Check a machine

```sh
uname -r                              # 6.0 or newer
cat /proc/sys/kernel/io_uring_disabled  # Linux 6.6+: 0 = allowed
ulimit -l                             # Linux 6.14-7.2: unlimited, or 150 KiB per worker
```

`kernel.io_uring_disabled` (Linux 6.6 and newer): `0` allows io_uring for every process,
`1` only for privileged processes and members of the group in `kernel.io_uring_group`,
`2` for nobody. Set it with `sysctl -w kernel.io_uring_disabled=0`, or put the service's
user in that group.

## Locked-memory limit

Linux 6.14 to 7.2 count the memory of every io_uring ring against the locked-memory limit
(`RLIMIT_MEMLOCK`, `ulimit -l`) of a process that lacks `CAP_IPC_LOCK`. Linux 7.3 stopped,
and so did 6.18 from 6.18.58; distribution kernels may have taken the change too. The
count is kept per user ID and summed over all of that user's processes, and the memory of
a closed ring is given back a moment after the close. A giong worker locks about 150 KiB
with the default settings: the submission and completion queues take 136 KiB of it
(`Builder::ring_entries`), the two rings of receive buffers 8 KiB each
(`Builder::recv_buffers`). The usual default limit of 8 MiB holds about fifty workers, for
all of the user's processes together; the 64 KiB of older systems holds none.

A worker that cannot lock the memory gets `ENOMEM` from the kernel, which giong reports as
[GIONG-E0010](errors/E0010.md), naming the limit; without an explicit driver it runs that
worker on epoll and says so once on stderr. Root is not held to the limit, since it has
`CAP_IPC_LOCK`, except where that capability was taken away, as in a Docker container.
Raise the limit, or lift it, for any service that is not root:

- systemd: `LimitMEMLOCK=infinity` in the `[Service]` section, or
  `AmbientCapabilities=CAP_IPC_LOCK` to lift it for a service with a `User=`.
- Docker: `docker run --ulimit memlock=-1:-1`, or in Docker Compose:

  ```yaml
  services:
    api:
      ulimits:
        memlock:
          soft: -1
          hard: -1
  ```

- Kubernetes has no setting for the limit. Add the capability where the pod's security
  policy allows it (`securityContext.capabilities.add: ["IPC_LOCK"]`; Pod Security's
  `baseline` and `restricted` levels do not), or raise the limit the container runtime
  starts containers with on each node (for containerd, `LimitMEMLOCK=infinity` in a
  drop-in for its systemd unit). Containers that run as the same user ID on one node share
  the count.
- A shell: `ulimit -l unlimited` as root before starting the service, or an entry in
  `/etc/security/limits.conf` for its user.

Or need less: fewer workers, a smaller `Builder::ring_entries`, fewer receive buffers. Or
run a kernel that does not count the rings.

## Ubuntu 24.04

Ubuntu's 6.8 kernels from 6.8.0-135 on have a bug that makes them refuse every correctly
formed io_uring buffer ring, which giong's io_uring driver needs: a backport inverted a
check on the request's reserved fields. The AWS, Azure, GCP and Oracle 6.8 kernels built
from them have it too, as does the 6.8 HWE kernel of Ubuntu 22.04. giong recognises the bug and registers its ring
the way those kernels accept, so io_uring works there at full speed, and
`giong::driver_choice()` lists the workaround (`buffer-ring-reserved-check`). Nothing to
do on your side. Other software that uses buffer rings through liburing fails on those
kernels with `EINVAL`. The 7.0 HWE kernel (`linux-generic-hwe-24.04`) does not have the
bug.

## Linux 6.0.16 to 6.0.19

The last four releases of the 6.0 series (which is no longer maintained) return 1
instead of -1 from bind(2) on a TCP socket whose address is in use, and leave the socket
unbound. Programs read that as success, and listen(2) then puts the socket on a free port
of every interface, so a server asked for a busy port starts on another one.
`giong::net::TcpListener::bind` checks which port the socket ended up on and fails with
`AddrInUse`, as on every other kernel. A listener that the standard library bound, handed
over with `TcpListener::from_std`, is not checked: compare its `local_addr()` with the
address you asked for, or let giong bind it. 6.0.15 and older, 6.1 and later, and UDP and
Unix sockets are not affected.

## Restarting on the same port

`TcpListener::bind`, `bind_balanced` and `UdpSocket::bind` let the workers of one process
share a port, and refuse it, with `AddrInUse`, while a socket of another process holds
it: a service started twice by mistake fails the second time instead of quietly taking
half of the first one's connections. What that means for a restart:

- Stopping the old process before starting the new one (systemd's `Restart=`, a container
  replaced by its orchestrator) needs nothing: the port of a closed listener can be bound
  again at once.
- Starting the new version on the same machine and port while the old one still serves
  fails until the old one has let the port go. Run the two on different ports behind the
  load balancer or proxy, or create the sockets outside giong: one per worker, with
  `SO_REUSEPORT` set (with the `socket2` crate, say) by every process that is to share the
  port, wrapped with `TcpListener::from_std` or `UdpSocket::from_std` in each worker.
  giong sets no option on such a socket and does not check who else holds its port.
- Connections still in a closing listener's accept queue are refused when it closes: have
  the load balancer stop sending to the old process first, and give it a
  `Builder::shutdown_grace` to answer what it already has.

## Sizing memory

What a giong-http server holds is bounded by its settings, per worker (each worker runs its
own `serve`):

```text
memory per worker  ≈  max_buffered_request_bytes
                    + open connections × (idle cost + up to 4 KiB while a request arrives)
                    + what the handlers hold
```

- `max_buffered_request_bytes` bounds the requests the worker is receiving in full: by
  default 64 MiB, or a quarter of the cgroup memory limit divided among the workers if that
  is less, so that one worker in a 128 MiB container takes 32 MiB at most. Past it, requests
  are refused with `503` (HTTP/1.1) or `REFUSED_STREAM` (HTTP/2) instead of being buffered
  ([GIONG-E1120](errors/E1120.md)). Bodies read as a stream (`Handler::stream_body`) hold
  one piece at a time and are not counted.
- An idle connection costs about 2 KiB over HTTP/1.1, 4.5 KiB over HTTP/2 and 11 KiB over
  HTTPS in the process, and the kernel about 4 KiB more per socket (charged to the
  container's memory under cgroup v2). `max_connections` (10,000 per `serve` call by
  default) bounds the count.

The defaults fit the budget to the container, but not the number of connections: 10,000
idle HTTPS connections take about 145 MiB with the kernel's share. In a small container,
lower `max_connections` to what fits next to the budget:

```rust
use giong_http::Server;

// One worker in a 256 MiB container, serving HTTPS: 64 MiB for requests being
// received, 5,000 connections x ~17 KiB = 83 MiB, and the rest for the program.
let server = Server::new()
    .max_buffered_request_bytes(64 << 20)
    .max_connections(5_000);
```

CI checks the bound: the `slow-uploads` job runs a server with one worker and with four in
a Docker container limited to 128 MiB, sends it hundreds of uploads that stop one byte short
of their announced length, over HTTP/1.1 and HTTP/2, and fails if the container is
OOM-killed or a well-behaved client mixed in stops getting answers
([scripts/slow-uploads.sh](../scripts/slow-uploads.sh)).

## Docker

Docker's default seccomp profile does not allow the io_uring system calls, so in a
container run with default settings giong uses its epoll driver. For io_uring, use a
profile that is Docker's default plus those three calls:

```sh
curl -fsSLo docker-default.json \
  https://raw.githubusercontent.com/moby/profiles/main/seccomp/default.json
jq '.syscalls += [{"names": ["io_uring_setup", "io_uring_enter", "io_uring_register"],
                   "action": "SCMP_ACT_ALLOW"}]' docker-default.json > giong-seccomp.json

docker run --security-opt seccomp=giong-seccomp.json your-image
```

With Docker Compose:

```yaml
services:
  api:
    image: your-image
    security_opt:
      - seccomp=giong-seccomp.json
```

This keeps every other restriction of the default profile. Avoid
`--security-opt seccomp=unconfined`, which turns all of them off.

Checked on Docker Engine 29.9.2 with a static binary that starts a runtime and does a TCP
round trip: with `giong-seccomp.json` it runs on io_uring. With the default profile,
giong's test suite runs on the epoll driver in CI (the `docker` job).

## Kubernetes

A pod's containers get the container runtime's default profile when their
`securityContext.seccompProfile.type` is `RuntimeDefault`, and also by default on
kubelets started with `--seccomp-default`. Pod Security's `restricted` level requires
either `RuntimeDefault` or `Localhost`. containerd's default profile blocks io_uring like Docker's does.

Install `giong-seccomp.json` from above on every node, under the kubelet's seccomp
directory (by default `/var/lib/kubelet/seccomp/`), for example as
`profiles/giong.json`, and point the container at it:

```yaml
securityContext:
  seccompProfile:
    type: Localhost
    localhostProfile: profiles/giong.json
```

The [Security Profiles Operator](https://github.com/kubernetes-sigs/security-profiles-operator)
can distribute the profile to the nodes for you. Managed platforms differ in what they
let you change; check that yours allows `Localhost` profiles.

## Sandboxes

Runtimes that put a user-space kernel between the container and Linux, such as gVisor
(used by GKE Sandbox), do not offer io_uring by default. giong runs on epoll there; run it
outside such a sandbox for io_uring.
