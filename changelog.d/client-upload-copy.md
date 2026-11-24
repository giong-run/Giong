### Changed

- **giong-http**: the client no longer copies a request, body included, every time it sends
  it over HTTP/1.1 (kept in case a closed keep-alive connection makes it send the request
  again): the request is lent to the connection and handed back whole. 200 uploads of 1 MiB
  to a giong-http server in one process ran 86.0 million instructions before and 55.0
  million after (callgrind), and each upload allocates 1 MiB less.
