# Use a database

A local database for development in one command, forward-only migrations, and the pool pattern every giong service uses. PostgreSQL is shown; MySQL and Redis work the same way.

## Start one for development

```sh
giong db up            # PostgreSQL, MySQL or Redis in Docker or Podman, for the crates the project uses
giong db new add_users # writes migrations/0002_add_users.sql
giong db migrate       # applies what is new
giong db status        # what is applied
```

`giong db up` gives the database a random password, a free port and a volume, and writes `DATABASE_URL` (or `REDIS_URL`) into `.env`, which only you can read and git ignores. Each checkout of the project gets its own container. `giong dev stop` stops it with the service and keeps its data; `giong db down` removes it, and `--volumes` deletes the data too. The [`giong db` page](../../giong-cli/README.md#giong-db) has every subcommand.

## Migrations

Migrations are plain SQL files, `migrations/NNNN_name.sql`, applied in order and never undone:

- Each PostgreSQL file runs in one transaction, unless its first line is `-- giong:no-transaction` (for `CREATE INDEX CONCURRENTLY` and the like).
- Each applied file is recorded with its SHA-256; changing a file that was already applied is refused. Write a new file instead.
- A file that fails names its line.
- With a `DATABASE_URL` of your own (staging, production), `giong db migrate` and `giong db status` work on that database.

## One pool per worker

Build the pool inside each worker's future, from the environment. Nothing is shared between threads, so nothing is locked:

```rust
use giong_postgres::{Config, Pool};

fn main() -> std::io::Result<()> {
    let url = std::env::var("DATABASE_URL").expect("DATABASE_URL");
    let config = Config::from_url(&url).expect("DATABASE_URL");
    giong::Builder::new().shutdown_on_signals(true).build()?.run(move |_worker| {
        let pool = Pool::new(config.clone(), 4);
        async move {
            let row = pool.query_one("SELECT count(*) FROM orders", &[]).await.unwrap();
            let n: i64 = row.get(0).unwrap();
            println!("{n} orders");
        }
    })
}
```

- `pool.query(..)`, `query_one`, `execute` share connections between tasks and pipeline their statements: a few connections serve hundreds of tasks.
- Transactions, `SET`, `LISTEN` and temporary tables need a connection of their own: `let mut conn = pool.get().await?;`. The pool's shared methods refuse them before sending anything. When `conn` is dropped, an open transaction is rolled back and the session is reset.
- Size the pool for the transactions that run at once, plus one. Spread the database server's cores over the workers rather than giving each worker many connections.
- Every wait has a limit by default: connecting, a statement, the pool (`GIONG-E3009` when it runs out).

The [giong-postgres](../../giong-postgres/README.md), [giong-mysql](../../giong-mysql/README.md) and [giong-redis](../../giong-redis/README.md) pages have the whole API, the types and the limits.

## Test against the real database

`giong test` reads `.env`, so the tests see the `DATABASE_URL` that `giong db up` wrote. A project from `giong new --db postgres` already has an end to end test that does this, and a CI workflow that starts the same database as a service.

## Not with sqlx or tokio-postgres

Crates that need the Tokio runtime (sqlx, tokio-postgres's runtime, most ORMs built on them) panic on a giong worker with "there is no reactor running". Use the giong crate for each database.
