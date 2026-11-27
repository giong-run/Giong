# The build store: one copy of each dependency build for every project

`giong cache share` (and `giong dev`, after each build) replaces the files a registry or git
dependency's build wrote with hard links to one copy in `~/.giong/store`, when the bytes are
the same. This measures what that saves on the builds of several projects from `giong new`,
and checks that cargo still sees every build as fresh afterwards.

## Result (2026-11-27)

Eight projects (`giong new`: two web + SQLite, web + Postgres, web + Postgres + Redis, web
with TLS and no store, gRPC, two workers), each built with `cargo build` and
`cargo test --no-run` (`results.txt`):

| | Disk used by the 8 `target/` directories |
|---|---|
| Before | 2,240,718,035 bytes (2.09 GiB) |
| After `giong cache share` | 1,512,893,393 bytes (1.41 GiB): **-32.4%** |

- `giong cache share` took 1.2 s for the eight projects (no hashing: a file is compared byte
  for byte only with the one copy of the same unit already in the store).
- Afterwards `cargo build` and `cargo test --no-run` rebuilt **0 units** in every project.
- Build time does not change: the store saves disk, not compilation (see *What it does not
  do*).

An earlier run on four of the projects (`cargo build` only, no tests): 645.8 MiB became
401.3 MiB, **-37.9%**, in 0.3 s, 0 units rebuilt.

After sharing, the eight projects' 1.41 GiB on disk split as:

| | MiB | Share |
|---|---|---|
| Dependency builds, one copy each | 543.0 | 37.6% |
| Dependency builds still per project (`OUT_DIR` of `ring` and `libsqlite3-sys`) | 52.8 | 3.7% |
| The projects' own crates: binaries, test binaries | 390.1 | 27.0% |
| Incremental-compilation data of the projects' own crates | 287.1 | 19.9% |
| Everything else cargo keeps (uplifted copies, dep-info, locks) | 169.8 | 11.8% |

So what is left is mostly each project's own code; the dependencies are almost all a single
copy now. Each further project on the same giong release costs about its own code plus the
dependencies no other project has.

## Why some dependency builds are still per project

- A file a build script wrote into `OUT_DIR` (the C code of `ring`, SQLite) is never shared:
  a build script that runs again may write into the file it finds there, which would change
  every project's copy. Only what rustc writes (libraries, metadata, proc-macro libraries,
  build-script programs) is shared, because rustc writes a new file and renames it over the
  old one (`rustc_replaces_its_outputs` in `giong-cli/src/store.rs` checks this on every
  platform CI runs).
- A crate whose build script generates code includes the project's own path in its build
  (serde's does), so its bytes differ per project, and so do the crates built on it with the
  same features. Different features give different bytes too. Measured on four projects:
  remapping the build directory's path to one name (`--remap-path-prefix`, what Cargo's
  unstable `trim-paths` would do) would raise the saving from 34.6% to 40.5% of all bytes.
  Not worth a per-project `RUSTFLAGS` that plain `cargo build` would not share.

## What it does not do

- It does not make builds faster: each project still compiles its dependencies once. Sharing
  compilation across projects needs either one build directory for all of them (Cargo's
  `build-dir` can be pointed at one, but stable cargo then takes one lock for every build of
  every project, and `cargo clean` in one project empties it for all), or a compiler cache such
  as sccache (which keeps its own copy of every output, so it adds disk instead of saving it).
  The store keeps each project independent and only removes the duplicate bytes.
- It handles cargo's build-dir layout only (cargo 1.100 and later). Older build directories
  are left as they are.

## Method

`run.sh <giong binary> <empty work dir>` makes the eight projects, builds them, measures the
bytes of the eight `target/` directories with `du -scb` (which counts a file with several
links once across all its arguments), runs `giong cache share` on the work directory with
`GIONG_STORE_DIR` inside it, measures again, then runs `cargo build --message-format=json`
and `cargo test --no-run --message-format=json` in each project and counts
`compiler-artifact` messages with `"fresh": false`.

Machine: Linux 6.18 VM, 4 vCPU, ext4; Rust 1.100.0; giong 1.5.4 from crates.io in the
projects; `giong` built from this branch in release.
