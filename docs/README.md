# giong's documentation

The [README](../README.md) is the place to start: what giong is for, the platforms, the
API at a glance, the design and the limitations. The same, page by page with a copy button
for agents, is on [giong.run/docs](https://giong.run/docs/). Each crate has a README of its own, linked
from the root one. The API reference is on [docs.rs](https://docs.rs/giong).

| Page | For |
|---|---|
| [BEST_PRACTICES.md](BEST_PRACTICES.md) | How to build a service on giong: the shape of a service, state, blocking, timeouts, buffers, errors, databases, HTTP clients, observing, deploying, testing, upgrading, and a checklist before you finish. |
| [BENCHMARKS.md](BENCHMARKS.md) | Every benchmark with its method, machine and raw output: throughput, latency, memory, bare metal, HTTPS, databases, gRPC, QUIC, the benchmark lab, and what has not been measured. |
| [STABILITY.md](STABILITY.md) | What the 1.x promise covers: the API with its documented behaviour (cancel safety included), error codes, environment variables, log format; what it leaves out; the MSRV and kernel rules. |
| [errors/](errors/README.md) | Every `GIONG-Exxxx` code: what went wrong and how to fix it. An error message names its page. |
| [DEPLOYMENT.md](DEPLOYMENT.md) | Running a giong service: checking a machine, the locked-memory limit, kernel quirks, restarting on the same port, sizing memory, io_uring in Docker, Kubernetes and sandboxes. |
| [design/portability.md](design/portability.md) | The driver boundary for Linux, macOS and Windows, and what still differs on macOS and Windows, area by area. |
| [kani.md](kani.md) | The Kani proofs of the `unsafe` core and how to run them. |
| [verus.md](verus.md) | The Verus proofs (no bounds): the IPC ring against any peer, a worker never forgets a task; how the proofs follow the code. |
| [DEVELOPMENT.md](DEVELOPMENT.md) | How a change goes from an item to a merge with several agents at once: preflight checks, changelog entries, avoiding conflicts in the merge queue, handoff. |
| [RELEASING.md](RELEASING.md) | How a release is cut and published (maintainers). |
| [reviews/](reviews/) | Written reviews: the 1.0 API freeze, API portability, giong-tls's security review, the gaps a general runtime still has. |

Elsewhere in the repository:

- [ROADMAP.md](../ROADMAP.md): the goal, the agent-first rules, the milestones and the
  non-goals.
- [CHANGELOG.md](../CHANGELOG.md): what changed in each release.
- [skills/giong](../skills/giong/SKILL.md): the giong skill for coding agents that write
  services on giong; giong.run serves it as a zip with every crate's README.
- [CONTRIBUTING.md](../CONTRIBUTING.md) and [AGENTS.md](../AGENTS.md): building, testing
  and changing giong, for people and for AI coding agents.
- [bench/](../bench/): the benchmark programs and scripts; recorded output in
  `bench/results/`, instruction counts in [bench/instr](../bench/instr/README.md).
- [sim/](../sim/README.md): the deterministic simulation used for fault injection and long
  runs.
