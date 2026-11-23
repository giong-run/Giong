### Fixed

- **giong**: a task receiving from an `mpsc` channel that another thread or worker keeps
  fed now yields to the rest of its worker after 128 values that were ready at once, as
  socket operations do; it used to hold the worker for as long as values kept coming, so
  a 10 ms timer on the same worker could fire 400 ms late. The first poll of a bounded
  `send` that finds room counts the same way
  ([#227](https://github.com/giong-run/Giong/issues/227)).
