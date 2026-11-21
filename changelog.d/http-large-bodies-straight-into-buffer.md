### Changed

- **giong-http**: a request body of 64 KiB or more is received straight into the request
  buffer, instead of being copied there out of the runtime's receive buffers. Uploads of
  1 MiB are 2.1 to 2.5 times as fast and use 51 to 59% less CPU, on both drivers;
  memory per idle connection and small requests are unchanged
  ([#194](https://github.com/giong-run/Giong/pull/194), `bench/results/upload-recv-into/`).
