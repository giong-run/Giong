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
[benchmarks](BENCHMARKS.md#io_uring-against-epoll) have the measured difference.

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
- An idle connection costs about 1.2 KiB over HTTP/1.1, 2.5 KiB over HTTP/2, 5.0 KiB over
  HTTPS and 6.2 KiB over HTTP/2 with TLS in the process, and the kernel about 4 KiB more per socket (charged to the
  container's memory under cgroup v2). `max_connections` (10,000 per `serve` call by
  default) bounds the count.

The defaults fit the budget to the container, but not the number of connections: 10,000
idle HTTPS connections take about 100 MiB with the kernel's share. Past what fits, the
server does not get killed: while less than `min_free_memory` is left to the process (a
tenth of the container's limit by default, between 16 and 256 MiB), it refuses new
connections with `503` and `Retry-After: 1`, or closes them before the TLS handshake
([GIONG-E1121](errors/E1121.md)), and keeps serving the ones it holds. That is a last line
of defence, which refuses clients the load balancer could have sent elsewhere. In a small
container, lower `max_connections` to what fits next to the budget, so that the server
stops accepting before memory runs low:

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
([scripts/slow-uploads.sh](../scripts/slow-uploads.sh)). The same job then runs it in 48 MiB
and opens idle connections until it refuses them, and fails if the container is
OOM-killed, if the connections it holds stop being served, or if it does not take new ones
once those close ([scripts/idle-connections.sh](../scripts/idle-connections.sh)).

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

## CPU limits

giong starts one worker per CPU the process may use: the CPUs it may run on (`--cpuset-cpus`,
the exclusive CPUs of Kubernetes' static CPU manager), fewer when a CPU quota allows less
(`--cpus`, `resources.limits.cpu`), rounded down, and at least one. A pod limited to 2 CPUs
on a 64-core node gets 2 workers. Tokio and `std::thread::available_parallelism` count the
same way.

A fraction is rounded down on purpose. Measured with the `defaults` example, wrk with 64
connections, io_uring ([raw data](../bench/results/containers/cpu-quota.txt)):

| Quota | Workers | Requests/s | p99 |
| --- | --- | --- | --- |
| 2 CPUs | 2 (default) | 285k | 0.55 ms |
| 2 CPUs | 4 | 248k | 4.1 ms |
| 1.5 CPUs | 1 (default) | 147k | 1.0-2.0 ms |
| 1.5 CPUs | 2 | 214k | 23 ms |
| 0.5 CPUs | 1 (default) | 70k | 50 ms |

More workers than the quota run until the quota is used up and then wait for the next
100 ms period: more requests per second, and a p99 10 to 20 times higher. Set
`Builder::workers` above the default only when throughput matters more than tail latency.
Under one CPU, the 100 ms period itself sets the p99 near 50 ms whatever the program does;
give a latency-sensitive service at least one whole CPU.

Kubernetes CPU requests without a limit set no quota: the process sees every CPU of the node,
and giong starts a worker for each. That costs little memory (about 0.4 MiB per worker under
load, [raw data](../bench/results/containers/memory-workers.txt)), but the workers share the
CPU time the pod gets. Pass the request to the program and use it, for example:

```yaml
env:
  - name: WORKERS
    valueFrom:
      resourceFieldRef:
        resource: requests.cpu   # whole CPUs, rounded up
```

```rust
let mut builder = giong::Builder::new();
if let Some(n) = std::env::var("WORKERS").ok().and_then(|n| n.parse().ok()) {
    builder = builder.workers(n);
}
```

## Thread and file limits

Each worker is a thread, and so is each of up to 64 threads that run file operations on the
epoll driver. A container's PID limit counts threads (`docker run --pids-limit`, the
kubelet's `podPidsLimit`, `TasksMax` in systemd): when it is reached, `Runtime::run`
returns [GIONG-E0020](errors/E0020.md), which names the worker that could not start and the
container's `pids.max`. Allow at least the workers, your own threads and 64 more.

Each connection is a file descriptor, and the soft `RLIMIT_NOFILE` (`ulimit -n`) bounds
them. containerd from 1.8 on, and so Kubernetes on most nodes today, starts containers with
the systemd default of 1,024 soft and 524,288 hard. At the soft limit giong-http's accept loop
waits 50 ms and tries again, serving the connections it has, while new clients wait in the
listen queue: measured with the `defaults` example in a container limited to 1,024 files,
1,071 of 1,100 keep-alive clients were answered and 29 were not.

So on Linux giong raises the soft limit to the hard one when a runtime is built, as Go
programs do: a process may do that without privileges, and with it all 1,100 clients are
answered in the same container. A hard limit of `unlimited` is read as `fs.nr_open`. To
keep the soft limit, for a program that starts children which still use `select(2)` and
need descriptors below 1,024 (children inherit the limit), build with
`Builder::raise_open_files_limit(false)`. A hard limit below what the service needs is
raised from outside: `docker run --ulimit nofile=65536:65536`, `ulimits` in Compose,
`LimitNOFILE` in systemd. When a server does run out, each worker says so on stderr at most
once a minute, with the limits it found ([GIONG-E1123](errors/E1123.md)), and giong-mcp's
`overview` shows the limits of the container (CPUs, CPU quota, memory, `pids.max`, open
files) beside where the worker count came from.

## Stopping: SIGTERM and process 1

`docker stop` and Kubernetes send `SIGTERM`, then `SIGKILL` 10 seconds later (Kubernetes:
`terminationGracePeriodSeconds`, 30 by default). A program that runs as process 1 of a
container gets no default action for `SIGTERM`: unless it handles the signal, it keeps
running until it is killed. With `Builder::shutdown_on_signals(true)` giong handles
`SIGINT` and `SIGTERM`, and with `shutdown_grace` the requests in flight are answered first;
the `rest-api` example stops in under half a second. Without it, run the container with
`--init` (`init: true` in Docker Compose), which forwards the signal and stops the process.

## Container image

[examples/rest-api/Dockerfile](../examples/rest-api/Dockerfile) builds the `rest-api` example
into a 44 MB image (distroless, glibc, no shell, an unprivileged user), and
[compose.yaml](../examples/rest-api/compose.yaml) runs it with PostgreSQL and Redis, a CPU
and a memory limit, and the lines that allow io_uring. Copy them for a service of your own:
giong needs nothing in the image besides the program.

## Sandboxes

Runtimes that put a user-space kernel between the container and Linux, such as gVisor
(used by GKE Sandbox), do not offer io_uring by default. giong runs on epoll there; run it
outside such a sandbox for io_uring. gVisor's optional io_uring (`runsc --iouring`) takes
none of the settings giong's rings need, so giong runs on epoll there too, with
[GIONG-E0001](errors/E0001.md) as the reason.

gVisor's network stack decides whether sockets may share a port when each one binds,
where Linux decides when the next one binds. giong finds out once per process which way
the system works, and its workers share a port either way. CI runs the tests that bind
one port from several workers, and a four-worker server, under gVisor
([scripts/gvisor.sh](../scripts/gvisor.sh)).
