-- The users table. `giong db migrate` applies the files of this directory in
-- order and remembers which it applied; the service also runs this one at
-- startup, so it only creates what is missing. Add changes as new files
-- (0002_add_column.sql...), never by editing an applied one.
CREATE TABLE IF NOT EXISTS users (
    id bigserial PRIMARY KEY,
    name text NOT NULL,
    email text NOT NULL UNIQUE,
    created_at timestamptz NOT NULL DEFAULT now()
);
