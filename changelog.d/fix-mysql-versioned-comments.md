### Fixed

- **giong-mysql**: statements inside versioned comments (`/*!40101 SET NAMES latin1 */`,
  MariaDB's `/*M!...*/`), which the server runs as SQL and dump tools write, are now
  refused on a pool's shared connections like the same statements written plainly. Before,
  the check skipped them as comments, and statements of other tasks pipelined behind one
  ran in the changed session until the reset
  ([#248](https://github.com/giong-run/Giong/issues/248)).
