//! Through PgBouncer in transaction mode, as managed PostgreSQL services often
//! run it in front of the server.
//!
//! PgBouncer is found through `GIONG_PGBOUNCER_BIN` (the binary), then on
//! `PATH` and in `/usr/sbin`. Without it these tests are skipped with a
//! message, unless `GIONG_PGBOUNCER_REQUIRED=1`, which makes that a failure
//! (CI sets it where it installs PgBouncer).

use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use giong::time::sleep;
use giong_postgres::{Client, Config, ErrorKind, Pool};

use crate::run;
use crate::server::{chown_tree, free_port, run_as, Server};

struct Bouncer {
    port: u16,
    child: Child,
    dir: PathBuf,
}

impl Drop for Bouncer {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

impl Bouncer {
    /// A client config for the server's superuser, through PgBouncer.
    fn config(&self) -> Config {
        Config::new()
            .host("127.0.0.1")
            .port(self.port)
            .user("postgres")
            .dbname("postgres")
    }
}

fn find_bin() -> Option<PathBuf> {
    if let Some(bin) = std::env::var_os("GIONG_PGBOUNCER_BIN") {
        return Some(PathBuf::from(bin));
    }
    let path = std::env::var_os("PATH").unwrap_or_default();
    std::env::split_paths(&path)
        .chain([PathBuf::from("/usr/sbin")])
        .map(|dir| dir.join("pgbouncer"))
        .find(|bin| bin.exists())
}

/// PgBouncer in transaction mode in front of the test server, with
/// `settings` added to its configuration and `users` (name and password) in
/// its auth file beside `postgres`; `None` (after saying why) when it is not
/// installed.
fn start(s: &Server, settings: &str, users: &[(&str, &str)]) -> Option<Bouncer> {
    let Some(bin) = find_bin() else {
        if std::env::var_os("GIONG_PGBOUNCER_REQUIRED").is_some_and(|v| v == "1") {
            panic!(
                "GIONG_PGBOUNCER_REQUIRED=1 but pgbouncer was not found; set GIONG_PGBOUNCER_BIN"
            );
        }
        eprintln!("skipping: pgbouncer not found (set GIONG_PGBOUNCER_BIN)");
        return None;
    };
    let port = free_port();
    let dir = std::env::temp_dir().join(format!("giong-pgbouncer-{}-{port}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let mut auth = "\"postgres\" \"\"\n".to_owned();
    for (user, password) in users {
        auth += &format!("\"{user}\" \"{password}\"\n");
    }
    std::fs::write(dir.join("users.txt"), auth).unwrap();
    std::fs::write(
        dir.join("pgbouncer.ini"),
        format!(
            "[databases]\n\
             postgres = host=127.0.0.1 port={} dbname=postgres\n\
             [pgbouncer]\n\
             listen_addr = 127.0.0.1\nlisten_port = {port}\nunix_socket_dir =\n\
             auth_type = trust\nauth_file = {d}/users.txt\n\
             pool_mode = transaction\ndefault_pool_size = 2\n\
             logfile = {d}/pgbouncer.log\n{settings}\n",
            s.port,
            d = dir.display()
        ),
    )
    .unwrap();
    // PgBouncer, like PostgreSQL, refuses to run as root.
    let owner = run_as();
    chown_tree(&dir, owner);
    let mut cmd = Command::new(bin);
    if owner.is_some() {
        cmd.args(["-u", "postgres"]);
    }
    let child = cmd
        .arg(dir.join("pgbouncer.ini"))
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("start pgbouncer");
    let bouncer = Bouncer { port, child, dir };
    let started = std::time::Instant::now();
    while std::net::TcpStream::connect(("127.0.0.1", port)).is_err() {
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "pgbouncer did not start: {}",
            std::fs::read_to_string(bouncer.dir.join("pgbouncer.log")).unwrap_or_default()
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    Some(bouncer)
}

/// 8 tasks, each with statements and transactions (into `table`) through
/// `pool`; returns the errors.
async fn workload(pool: &Pool, table: &'static str) -> Vec<String> {
    pool.batch_execute(&format!(
        "DROP TABLE IF EXISTS {table}; CREATE TABLE {table} (task int, i int)"
    ))
    .await
    .unwrap();
    let tasks: Vec<_> = (0..8i32)
        .map(|t| {
            let pool = pool.clone();
            giong::spawn(async move {
                let mut errors = Vec::new();
                for i in 0..50i32 {
                    match pool
                        .query_one("SELECT $1::int4 + 1", &[&(t * 1000 + i)])
                        .await
                    {
                        Ok(row) => assert_eq!(row.get::<i32>(0).unwrap(), t * 1000 + i + 1),
                        Err(e) => errors.push(e.to_string()),
                    }
                    if i % 10 == 0 {
                        let result = async {
                            let mut conn = pool.get().await?;
                            let tx = conn.transaction().await?;
                            let insert = format!("INSERT INTO {table} VALUES ($1, $2)");
                            tx.execute(&insert, &[&t, &i]).await?;
                            tx.commit().await
                        }
                        .await;
                        if let Err(e) = result {
                            errors.push(e.to_string());
                        }
                    }
                }
                errors
            })
        })
        .collect();
    let mut errors = Vec::new();
    for t in tasks {
        errors.extend(t.await.unwrap());
    }
    errors
}

#[test]
fn through_pgbouncer_the_statement_timeout_must_not_be_sent() {
    run(|s| async move {
        let Some(bouncer) = start(s, "max_prepared_statements = 100", &[]) else {
            return;
        };
        // PgBouncer refuses startup parameters it does not track.
        let e = Client::connect(&bouncer.config()).await.unwrap_err();
        assert_eq!(e.kind(), ErrorKind::Connect, "{e}");
        assert_eq!(e.code(), Some("08P01"), "{e}");
        let shown = e.to_string();
        assert!(shown.contains("statement_timeout(None)"), "{shown}");
        assert!(shown.contains("ALTER ROLE"), "{shown}");

        // Without it, pipelined statements, prepared statements (PgBouncer
        // keeps them per client) and transactions work, for any cache size,
        // and the pool's session reset passes through.
        for cache in [256, 0] {
            let config = bouncer
                .config()
                .statement_timeout(None)
                .statement_cache_capacity(cache);
            let pool = Pool::new(config, 4);
            let errors = workload(&pool, "bouncer_log").await;
            assert!(errors.is_empty(), "cache {cache}: {errors:?}");
        }
    });
}

#[test]
fn through_pgbouncer_the_statement_timeout_comes_from_the_role() {
    run(|s| async move {
        s.psql(
            "DROP ROLE IF EXISTS bouncer_user; \
             CREATE ROLE bouncer_user LOGIN PASSWORD 'bouncer-secret'; \
             ALTER ROLE bouncer_user SET statement_timeout = '300ms'; \
             GRANT CREATE ON SCHEMA public TO bouncer_user",
        );
        // PgBouncer 1.22 accepts statement_timeout in the startup message
        // once it is in track_extra_parameters, but never passes it on to
        // the server: the limit would be lost without a word. Set on the
        // role, it holds, and the pool's RESET ALL returns to it.
        let Some(bouncer) = start(
            s,
            "max_prepared_statements = 100",
            &[("bouncer_user", "bouncer-secret")],
        ) else {
            return;
        };
        let config = bouncer
            .config()
            .user("bouncer_user")
            .statement_timeout(None);
        let pool = Pool::new(config, 2);
        {
            let conn = pool.get().await.unwrap();
            conn.batch_execute("SET statement_timeout = 0")
                .await
                .unwrap();
        }
        for _ in 0..4 {
            let row = pool.query_one("SHOW statement_timeout", &[]).await.unwrap();
            assert_eq!(row.get::<&str>(0).unwrap(), "300ms");
        }
        let e = pool.query("SELECT pg_sleep(5)", &[]).await.unwrap_err();
        assert_eq!(e.code(), Some("57014"), "{e}");
        let errors = workload(&pool, "bouncer_role_log").await;
        assert!(errors.is_empty(), "{errors:?}");
        sleep(Duration::from_millis(1)).await;
    });
}
