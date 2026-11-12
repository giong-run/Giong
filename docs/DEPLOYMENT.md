# Deploying giong

giong runs on Linux 6.0 or newer (6.1 or newer recommended) and needs the three io_uring
system calls: `io_uring_setup`, `io_uring_enter` and `io_uring_register`. Many container
platforms block them by default. When that happens, starting the runtime fails with an
error that begins with `giong needs io_uring, which is not available here`.

This page shows how to allow io_uring where it is blocked. A fallback to epoll for such
environments is planned (see [ROADMAP.md](../ROADMAP.md)).

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

Docker's default seccomp profile does not allow the io_uring system calls, so giong fails
to start in a container run with default settings. Use a profile that is Docker's default
plus those three calls:

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
round trip: with the default profile it fails with the error above, with
`giong-seccomp.json` it succeeds.

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
(used by GKE Sandbox), do not offer io_uring by default. Run giong outside such a sandbox.
