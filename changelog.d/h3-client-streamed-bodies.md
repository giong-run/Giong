### Added

- **giong-quic**: `SendStream::stopped` waits for the peer to ask for no more data
  (`STOP_SENDING`) and gives its code, so a sender with nothing to write yet learns it without
  writing.
- **giong-http**: the HTTP/3 client sends streamed request bodies (`RequestBuilder::streamed_body`)
  with their trailer fields, full duplex: the response is read while the body is still being
  written, and what is left of the body after the response head goes out from a task of its own.
  A body the producer aborts resets the stream with `H3_REQUEST_CANCELLED`; a server that answers
  without reading (STOP_SENDING with `H3_NO_ERROR`) gets no more of it and the producer is told;
  a request rejected before anything of its body was taken is sent again. Such requests used to
  be refused with prior knowledge and sent over TCP otherwise.
