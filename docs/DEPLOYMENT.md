# Deploying giong

giong is fastest on io_uring, which needs Linux 6.0 or newer (6.1 or newer recommended)
and the three io_uring system calls: `io_uring_setup`, `io_uring_enter` and
`io_uring_register`. Many container platforms block them by default.

Where io_uring cannot be used, giong runs on its **epoll driver** instead, without being
told: in a Docker container with the default seccomp profile, in Kubernetes with the
`RuntimeDefault` profile, under gVisor, with the `kernel.io_uring_disabled` sysctl set,
or on a kernel older than 6.0. Every API works the same way on both drivers; epoll makes
more system calls per request, and file operations run on a pool of blocking threads.
`giong::driver_kind()` tells which driver a worker got; log it at startup if you want to
know. The [README](../README.md#two-drivers-io_uring-and-epoll) has the measured
difference.

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
```

`kernel.io_uring_disabled` (Linux 6.6 and newer): `0` allows io_uring for every process,
`1` only for privileged processes and members of the group in `kernel.io_uring_group`,
`2` for nobody. Set it with `sysctl -w kernel.io_uring_disabled=0`, or put the service's
user in that group.

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
