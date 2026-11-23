### Fixed

- **giong** (Windows): a socket dropped while the driver was busy is closed at the next
  turn of the event loop, as on the other platforms. It could stay open, keeping a
  listener's port and leaving the peer without an end of stream, until some other close or
  cancel was deferred, and was never closed if a runtime ended first
  ([#289](https://github.com/giong-run/Giong/issues/289)).
