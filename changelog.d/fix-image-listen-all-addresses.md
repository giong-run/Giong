### Fixed

- **giong-cli**: an image from `giong image` now runs a service from `giong new` the way its
  Dockerfile does. It sets `LISTEN=0.0.0.0:<port>`: without it the service listened on
  `127.0.0.1` inside the container, so `docker run -p 8080:8080`, which `giong image` prints,
  reached nothing. For a SQLite project (`DATABASE_URL=sqlite:...` in `.env.example`, the
  default of `giong new`) it keeps the file in a `/data` volume owned by nonroot, with
  `DATABASE_URL=sqlite:///data/<file>`: before, the service exited at start with
  "Permission denied", since nonroot cannot write to `/app`. `docker run -e` still sets other
  values.
