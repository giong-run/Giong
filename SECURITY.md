# Security policy

## Reporting a vulnerability

Report it privately, through a GitHub security advisory:
<https://github.com/giong-run/Giong/security/advisories/new>. Please do not open a public
issue, pull request or discussion for it.

Put in the report:

- the giong version (or `main` commit), the crate and its features;
- the platform: Linux kernel version and driver (`io_uring` or `epoll`, `GIONG_DRIVER`),
  macOS, or Windows;
- a reproducer: a program, the bytes a peer sends, or the steps, and what happens (a crash,
  a hang, memory that keeps growing, data that reaches the wrong place);
- what an attacker needs: a network peer, a database server, a local user, a file in a
  project.

A report is checked against the threat model below. A report we cannot reproduce on a
released version or on `main` (for example the output of a scanner or an AI tool with no
working reproducer) is closed with that reason; send it again with a reproducer.

The fix is made in the advisory's private fork, with a test that fails without it, and
ships as a patch release of the latest 1.x version. The advisory is then published on
GitHub (GHSA, with a CVE when the issue warrants one) and sent to the RustSec advisory
database, so `cargo audit` and `cargo deny` report affected versions. The reporter is
credited unless they ask not to be. The release's `CHANGELOG.md` entry lists the fix under
**Security**.

## Supported versions

Fixes go into the **latest 1.x minor release**, as a new patch release (1.N.x). Older minor
releases get no backports: within major version 1 every upgrade keeps code building and
behaving the same ([docs/STABILITY.md](docs/STABILITY.md)), so the way to a fix is the latest
1.x. The kernels, Rust versions and platforms supported are those in
[docs/STABILITY.md](docs/STABILITY.md).

| Version | Security fixes |
|---|---|
| latest 1.x | yes |
| older 1.x | no: upgrade to the latest 1.x |
| 0.x and release candidates | no |

## Threat model

giong is a runtime and a set of crates that a service is built from. The question this
section answers is: when a service built on giong is exposed to the inputs below, what
does giong promise, so that breaking that promise is a vulnerability?

### What giong defends against (in scope)

- **Memory safety.** Undefined behaviour reached through the safe API, with any input,
  on any supported platform and driver: out-of-bounds or use-after-free accesses (including
  memory the kernel writes into after the operation that lent it was dropped), data races,
  double frees. The `unsafe` code in every crate is in scope.
- **Remote network peers** of the HTTP/1.1, HTTP/2 and HTTP/3 servers and clients, QUIC,
  TLS, gRPC, the DNS resolver and giong-web. A peer must not be able to:
  - crash the process or a worker, or make a worker hang;
  - make the process hold memory, connections, streams or CPU past the documented limits
    (for example `max_head_size`, `max_body_size`, `max_buffered_request_bytes`,
    `max_concurrent_streams`, the HTTP/2 memory and reset budgets, `max_connections`, and
    `min_body_rate`, `max_connections_per_ip` and `first_request_timeout` when set), or keep
    a resource past its documented timeout;
  - smuggle a request past the parser (request desync), or make one request's data or
    headers reach another request or connection;
  - get past certificate or name checks, or downgrade TLS below what the configuration
    allows.
- **Servers giong connects to**: PostgreSQL, MySQL, Redis, HTTP upstreams. A malicious or
  compromised server must not cause undefined behaviour, a panic or unbounded memory in the
  client, and a password must not cross the network in a form the documentation says it
  does not (for example in clear text where SCRAM or TLS is promised).
- **Other processes on giong::ipc channels.** A peer that writes anything into the shared
  ring (including counters that lie) must not cause undefined behaviour in the safe
  receive path; it is reported (`GIONG-E0091`). `Receiver::recv_in_place` is `unsafe` and
  is for peers you trust.
- **Paths opened through `giong::fs::Root`** (a request path, an upload's file name). Such a
  path must not open, create, empty or stat anything that is not beneath the root's
  directory: not with `..`, an absolute path, a symbolic link or junction, or a rename that
  races the lookup. It is refused (`GIONG-E0205`). Paths given to `File::open` and the other
  `fs` functions are opened as given.
- **giong-mcp.** Reachable only as documented: a Unix socket of mode `0600`, or TCP with a
  bearer token (a server without one does not start unless the code opts out). Its tools
  look and change nothing in the service.
- **The dev kit (`giong` CLI)** against files a project commits (`.giong/*.json`,
  `.env`, recorded tests, symbolic links). A command that only reads (`giong dev status`,
  `giong routes`, the read-only MCP tools) must not run a program, write outside the
  project or send a secret to another host because of such a file, and no command may
  follow a committed symbolic link out of the project when it writes. `giong mcp` serves
  only read-only tools unless started with `--allow-writes`. Dev servers listen on
  loopback unless asked otherwise, and dev features are compiled out of release builds.
- **What we ship**: a crate on crates.io or a release binary that does not match the
  tagged source, or a download path (`install.sh`, `giong upgrade --self`) that accepts a
  file it did not verify.

### What is not a vulnerability (out of scope)

- **Code in the same process.** Handlers, dependencies and every task of the process are
  trusted. Budgets (`giong::budget`), lanes, limits and timeouts are resource controls
  against overload and noisy neighbours, not a security boundary between pieces of code in
  one process. For untrusted code, use an operating-system boundary: a separate process,
  a container, seccomp, a VM.
- **Building or running a project.** `giong dev`, `giong test`, `giong new` followed by a
  build, and `cargo build` run the project's own code (build scripts, the service). Running
  them on a project you do not trust is running untrusted code.
- **Volume.** More traffic than the machine can serve. giong refuses work at its limits
  (and below `min_free_memory`) instead of failing, but no limit makes a server's capacity
  infinite. Limits set by the user to values that remove them (`usize::MAX`, a turned-off
  check) are the user's choice.
- **Local attackers with the same user id or root**, who can already read the process's
  memory, and bugs in the kernel itself (giong works around the ones it knows,
  `KernelWorkaround`; report the kernel bug upstream).
- **Cryptographic primitives and their side channels**, which belong to rustls, ring and
  aws-lc-rs: report those upstream (we will help).
- **`unsafe` functions used against their documented contract**, items marked
  `#[doc(hidden)]`, code built only with `--cfg fuzzing`, and features named `unstable-*`
  used outside what they document.
- **A panic in a handler.** giong-http answers `500` and keeps serving; the panic itself is
  the application's bug. (With `RUST_BACKTRACE` set, the panic hook symbolizes a backtrace
  on the worker: see [docs/BEST_PRACTICES.md](docs/BEST_PRACTICES.md).)

## How giong is checked

What is run, and how often, is listed in the README under
[Testing and safety](README.md#testing-and-safety); the parts that bear on security:

- every parser that reads bytes from a peer has a fuzz target, fuzzed every night in CI
  with a corpus that keeps growing;
- the unsafe code runs under Miri on every merge (the core crate, giong-http's allocator
  and parsers, the database clients' read buffers), under AddressSanitizer and
  LeakSanitizer every night (every test of the workspace) and under ThreadSanitizer on
  every merge; Kani proves the core's buffer and operation lifetimes;
- dependencies: `cargo deny` checks licenses and sources on every change and RustSec
  advisories every day (`deny.toml`, `.github/workflows/supply-chain.yml`);
- releases: crates are published through crates.io Trusted Publishing from the release
  workflow, whose actions are pinned by commit hash. Each release archive, and the `giong`
  binary in it, has a `.sha256`, a signed build provenance attestation and a CycloneDX
  SBOM; `gh attestation verify <file> --repo giong-run/Giong --signer-workflow
  giong-run/Giong/.github/workflows/release.yml` shows it was built by the release workflow
  from the tagged commit. The binary also lists the crates compiled into it (cargo
  auditable), so `cargo audit bin` and other scanners can check it for advisories.
