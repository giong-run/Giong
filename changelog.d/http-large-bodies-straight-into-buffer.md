### Changed

- **giong-http**: a request body of 64 KiB or more is received straight into the request
  buffer, instead of being copied there out of the runtime's receive buffers. Uploads of
  1 MiB are 2.2 to 2.3 times as fast and use 53 to 58% less CPU, on both drivers (3 MiB:
  2.4 to 5.2 times), and small requests on the same connections are 1.2 to 1.4 times as
  fast; memory per idle connection and small requests alone are unchanged
  ([#194](https://github.com/giong-run/Giong/pull/194), `bench/results/upload-recv-into/`).
