# Build time, disk and binary size: giong-web against axum

`bench/build.sh` on a 4-vCPU cloud VM, Rust 1.97 (default linker, rust-lld), 2026-11-22.
The same three-route JSON service with the same layers (request ids, a log line per
request, compression, a 10 s timeout): giong-web 1.3.0 from crates.io (`bench/build-giong`)
against axum 0.8 + tokio + tower-http 0.6 (`bench/build-axum`), crates fetched before
timing. Mean of two rounds (the raw file also holds a first giong block that failed on a
missing feature, fixed before the two rounds counted here). One VM, so read the times
as ±10%; sizes are exact.

| | giong-web | axum + tokio + tower-http | |
|---|---|---|---|
| Crates in the build (normal dependencies and the app, `cargo tree -e normal`) | **22** | 83 | 3.8× fewer |
| Clean debug build | **7.4 s** | 18.8 s | 2.5× faster |
| Rebuild after `touch src/main.rs` | **0.33 s** | 0.59 s | |
| Rebuild after a one-line edit | **0.56 s** | 0.87 s | 1.6× faster |
| `target/` after a debug build | **181 MB** | 403 MB | 2.2× smaller |
| Debug binary | 23.9 MB | 48.0 MB | |
| `debug = "line-tables-only"`: clean / edit / `target/` | 6.9 s / 0.43 s / 140 MB | 18.5 s / 0.65 s / 318 MB | |
| Clean release build (default release profile) | **11.4 s** | 33.9 s | 3.0× faster |
| Release rebuild after an edit | 2.4 s | 3.5 s | |
| `target/` after a release build | 68 MB | 190 MB | |
| Release binary, stripped | **1.45 MB** | 3.60 MB | 2.5× smaller |
| `opt-level = "s"`, `panic = "abort"`, stripped | 1.07 MB | 2.77 MB | |

- giong's stack is its own crates with no macros in the runtime or the framework; the
  one proc-macro crate the giong app builds is `serde_derive`, for its own types.
- For a service this small an edit already rebuilds in under a second on both. The gap
  grows with clean builds (CI, a new checkout, a Docker build without a cache) and with
  disk. A larger service has not been measured.
