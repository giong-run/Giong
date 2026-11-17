# giong's documentation

The [README](../README.md) is the place to start: what giong is for, the API at a glance,
the design, every benchmark and the limitations. Each crate has a README of its own, linked
from the root one. The API reference is on [docs.rs](https://docs.rs/giong).

| Page | For |
|---|---|
| [STABILITY.md](STABILITY.md) | What the 1.x promise covers: the API with its documented behaviour (cancel safety included), error codes, environment variables, log format; what it leaves out; the MSRV and kernel rules. |
| [errors/](errors/README.md) | Every `GIONG-Exxxx` code: what went wrong and how to fix it. An error message names its page. |
| [DEPLOYMENT.md](DEPLOYMENT.md) | Running a giong service: checking a machine, the locked-memory limit, kernel quirks, restarting on the same port, sizing memory, io_uring in Docker, Kubernetes and sandboxes. |
| [kani.md](kani.md) | The Kani proofs of the `unsafe` core and how to run them. |
| [RELEASING.md](RELEASING.md) | How a release is cut and published (maintainers). |
| [reviews/](reviews/) | Written reviews: the 1.0 API freeze, API portability, giong-tls's security review, the gaps a general runtime still has. |

Elsewhere in the repository:

- [ROADMAP.md](../ROADMAP.md): the goal, the agent-first rules, the milestones and the
  non-goals.
- [CHANGELOG.md](../CHANGELOG.md): what changed in each release.
- [CONTRIBUTING.md](../CONTRIBUTING.md) and [AGENTS.md](../AGENTS.md): building, testing
  and changing giong, for people and for AI coding agents.
- [bench/](../bench/): the benchmark programs and scripts; recorded output in
  `bench/results/`, instruction counts in [bench/instr](../bench/instr/README.md).
- [sim/](../sim/README.md): the deterministic simulation used for fault injection and long
  runs.
