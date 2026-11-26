### Fixed

- **giong-cli**: an image from `giong image` now sets `LISTEN=0.0.0.0:<port>`, as the
  Dockerfile from `giong new` does. Without it a service from `giong new` listened on
  `127.0.0.1` inside the container, so `docker run -p 8080:8080`, which `giong image` prints,
  reached nothing. `docker run -e LISTEN=...` still sets another address.
