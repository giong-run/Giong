### Changed

- **giong-tls** (performance): an idle TLS connection, rustls in the process, holds 6.1 KiB
  of heap instead of 10.8 (−44%, `bench/instr`'s `idle-https`); 8,000 idle HTTPS connections
  to one giong-http worker take 52.8 MiB of resident memory instead of 89.8. rustls now runs
  through its unbuffered API: received bytes are processed from a scratch buffer each thread
  shares, and a connection keeps bytes of its own only while it holds part of a record,
  where rustls' buffered connection kept a 4 KiB receive buffer for the life of every
  connection. Outgoing records go through scratch buffers each thread shares as well. HTTPS
  requests take 1.1% fewer instructions, and `bench/tls.sh` measured no loss on any endpoint.
  A client that sends 0-RTT data to an acceptor made with `TlsAcceptor::from_rustls` and a
  `max_early_data_size` above 0 now gets a `GIONG-E2005`-coded refusal, as with kernel TLS,
  where giong-tls never read that data. Method and raw data:
  `bench/results/tls-idle/`.
