### Fixed

- **giong-http**: an HTTP/2 client that opens many streams before it has the server's
  SETTINGS is no longer cut off. Until then it cannot know `max_concurrent_streams`
  (RFC 9113, section 6.5.2): the streams past the limit are still refused, but the
  refusals count as frames that do no work instead of resets, so 1,000 streams in a first
  flight get 900 refusals and 100 answers instead of `GOAWAY` `GIONG-E1116` after 201.
  Opening past the limit after the SETTINGS acknowledgement still counts as before. A
  client that has not acknowledged the server's SETTINGS within 10 seconds now gets
  `GOAWAY` `SETTINGS_TIMEOUT` (section 6.5.3), so none can keep the more lenient
  accounting ([#234](https://github.com/giong-run/Giong/issues/234)).
