### Security

- **giong-http**: an HTTP/2 SETTINGS frame moves the stream windows once, by the last
  `SETTINGS_INITIAL_WINDOW_SIZE` it holds, instead of once per value. A frame of the largest
  default size holds 2,730 of them: with 100 streams open, a client made each frame cost
  273,000 stream updates, about 2 seconds of a worker per connection for the frames the flood
  limit lets through. The check that no window passes 2^31-1 still takes the largest value in
  the frame, as processing the values in order would
  ([#233](https://github.com/giong-run/Giong/issues/233)).
