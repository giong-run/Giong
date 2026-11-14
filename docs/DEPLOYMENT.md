# Deploying giong

giong is fastest on io_uring, which needs Linux 6.0 or newer (6.1 or newer recommended)
and the three io_uring system calls: `io_uring_setup`, `io_uring_enter` and
`io_uring_register`. Many container platforms block them by default.

Where io_uring cannot be used, giong runs on its **epoll driver** instead, without being
told: in a Docker container with the default seccomp profile, in Kubernetes with the
`RuntimeDefault` profile, under gVisor, with the `kernel.io_uring_disabled` sysctl set,
on a kernel older than 6.0, or on Linux 6.14 and newer when the
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
ulimit -l                             # Linux 6.14+: unlimited, or about 150 KiB per worker
```

`kernel.io_uring_disabled` (Linux 6.6 and newer): `0` allows io_uring for every process,
`1` only for privileged processes and members of the group in `kernel.io_uring_group`,
`2` for nobody. Set it with `sysctl -w kernel.io_uring_disabled=0`, or put the service's
user in that group.

## Locked-memory limit

Since Linux 6.14 the kernel counts the memory of every io_uring ring against the
locked-memory limit (`RLIMIT_MEMLOCK`, `ulimit -l`) of a process that lacks
`CAP_IPC_LOCK`. The count is kept per user ID and summed over all of that user's
processes, and the memory of a closed ring is given back a moment after the close. A
giong worker locks about 150 KiB with the default settings: the submission and
completion queues take 136 KiB of it (`Builder::ring_entries`), the two rings of receive
buffers 8 KiB each (`Builder::recv_buffers`). The usual default limit of 8 MiB holds about
fifty workers, for all of the user's processes together; the 64 KiB of older systems
holds none.

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

Or need less: fewer workers, a smaller `Builder::ring_entries`, fewer receive buffers.

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
