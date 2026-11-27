# A project's first build, filled from the builds of other projects

Before a dev build, `giong dev` (and `giong cache fill`) gives the project's `target/debug`
the dependency builds other projects on the machine already made, so cargo compiles only
what is left. This measures what that does to the first build of new projects, and checks
that nothing else gets slower.

## Result (2026-11-27)

Eight projects from `giong new`, made and built one after another (`results.txt`). Each
one: `cargo build` (its first dev build), `cargo test --no-run` (its first test build), an
edit to `src/main.rs` and `cargo build` again, then `cargo test` (must pass). "cargo" is
plain cargo. "giong" runs `giong cache fill` before the first build and `giong cache
share` after it, which is what `giong dev` does around its builds.

| Project (in order) | cargo: first build + test build | giong: fill + first build + test build | Crates compiled, cargo → giong |
|---|---|---|---|
| shop (web, SQLite) | 22.1 s | 23.9 s | 66 → 66 (nothing to take from yet) |
| billing (web, SQLite) | 23.6 s | **3.1 s** | 66 → 7 |
| notes (web, Postgres) | 16.1 s | **4.2 s** | 56 → 8 |
| orders (web, Postgres, Redis) | 16.1 s | **4.2 s** | 57 → 8 |
| feed (web, TLS) | 17.3 s | **8.9 s** | 55 → 13 |
| search (gRPC) | 15.1 s | 9.9 s | 78 → 46 (first gRPC project) |
| jobs (worker, Postgres) | 10.6 s | **2.8 s** | 39 → 6 |
| mailer (worker, Redis) | 10.1 s | **1.6 s** | 39 → 5 |
| **All eight** | **131.0 s** | **58.7 s (-55%)** | |
| The seven after the first | 109.0 s | 34.7 s (-68%) | |

- Filling takes 0.1 to 0.15 s per project.
- Edit and rebuild does not change: 0.21 to 0.33 s in both columns.
- Disk for the eight `target/` directories: 2.24 GB with cargo, 1.32 GB with giong
  (-41%, with `giong cache share`).
- `cargo test` passes in every project, and a service started from a filled build serves
  requests.

A project gains the most when an earlier one used the same dependencies. The first gRPC
project and the first project with TLS still compile what no earlier project had.

## Why cargo can use another project's build

Cargo names each unit's directory (`target/debug/build/<package>/<hash>/`) after its own
hash of everything that goes into the unit: package and version, features, profile, target,
compiler, flags, and the same for each dependency. Inside, the fingerprint file holds the
hash cargo checks before it uses a build. When a project asks for the same unit, it asks
for the same directory. Cargo still compares the fingerprint, and builds again anything that
differs (another compiler, other features, another environment for a build script). So
`fill` only adds directories, and cargo decides whether to use them.

What `fill` takes:

- Only units of registry and git dependencies whose shared times are already set. Never a
  project's own crates, never units the project already has, never a release build.
- A unit only together with each of its dependencies from the same project, or when the
  dependency here already has the same bytes. A crate is compiled against the exact bytes of
  its dependencies, so mixing builds from different projects could otherwise give rustc a
  library that does not match.
- Libraries are hard links (on Windows, DLLs and programs are copies). Build-script output
  and cargo's own small files are copies, because a build may write into them. Paths a build
  script printed (`rustc-link-search`) are moved to the new project by cargo itself, which
  records where the output was first made.
- From the project with the most of the packages first, then up to 7 more for packages that
  are still missing.

## What it does not do

- It does not speed up compiling. The second project skips compiling because the first one
  already paid for it.
- On Linux x86-64, cargo has linked with lld by default since Rust 1.90, and the `giong new`
  templates already set `debug = "line-tables-only"` (`giong doctor` suggests it for older
  projects). giong changes neither the linker nor the profile. Changing either from `giong
  dev` alone would give cargo other hashes than a plain `cargo build`, and the two would
  rebuild each other's work.

## Method

`run.sh <giong binary> <empty work dir>` (see the script header). Machine: Linux 6.18 VM,
4 vCPU, ext4; Rust 1.100.0; giong 1.5.4 from crates.io in the projects; `giong` built in
release from this branch. The crates were already downloaded, so no time went to the
network.
