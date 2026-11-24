### Changed

- **giong**: lookups of one name that run on a worker at the same time share one pair of
  queries: when a cached name expires under load, 500 requests that need it send 2 queries,
  not 1,000. A name that does not exist is remembered as missing for its zone's negative
  TTL (the SOA record of the answer, RFC 2308, at most 30 seconds), so a client retrying a
  mistyped host no longer sends two queries per attempt. And once one of the `A` and
  `AAAA` queries has answered with addresses, the other is waited for only 50 ms (or as
  long as the first answer took, if longer): a DNS server that never answers `AAAA`
  queries used to hold the `A` records for the whole timeout, 5 seconds by default, on
  every lookup. Addresses of one family found that way are cached for at most 30 seconds.
  Fixes [#265](https://github.com/giong-run/Giong/issues/265).

### Fixed

- **giong**: on Windows, the resolver's query sockets bind their port exclusively
  (`SO_EXCLUSIVEADDRUSE`). Before, another socket of any process could bind the same port
  on a more specific address, such as `127.0.0.1`, and take the answers sent to it: the
  lookup then waited out its timeout, or lost one address family.
