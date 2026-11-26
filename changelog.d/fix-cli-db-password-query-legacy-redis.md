### Security

- **giong-cli**: `giong db` also keeps two other forms of password off a client's command
  line: a `password=` in a PostgreSQL URL's query (given to `psql` in `PGPASSWORD`, winning
  over one before the host as libpq does) and the older `redis://secret@host` form with no
  user (given to `redis-cli` in `REDISCLI_AUTH`).
