### Security

- **giong-cli**: `giong upgrade --self` and the install script refuse a redirect from an
  https download to plain http and ask for TLS 1.2 or later, so a network in the middle can
  never hand over another archive together with its matching `.sha256`. A mirror named
  with `http://` in `GIONG_DOWNLOAD_URL` still works.
- **all crates**: a supply-chain gate (`deny.toml`, cargo-deny): RustSec advisories are
  checked every day and on every dependency change, licenses must be permissive and crates
  come from crates.io only. The test client's `quinn-udp` moves to 0.5.16 for
  RUSTSEC-2026-0538 (a development dependency only; no published crate used it).
