### Added

- **giong-web**: the `fast-gzip` feature (off by default) has `Compression` gzip bodies of
  up to 16 KiB at level 1, the default, with libdeflate in one call. On an 8 KiB JSON
  response, measured on bare metal: 31% less server CPU per request, 46% more requests per
  second on one worker and 43.5% more on four, a lower p99 at every load, and 4% smaller
  bodies. Responses without gzip are unchanged. Larger bodies and other levels keep the
  pure Rust compressor. The binary grows by about 90 KiB and building needs a C compiler. A
  new fuzz target checks both compressors against an independent decoder. Contributed by concacotdua
  ([#429](https://github.com/giong-run/Giong/pull/429)).
