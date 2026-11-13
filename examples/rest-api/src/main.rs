//! Starts the API: reads the configuration from the environment, creates
//! the table, then serves on every core.
//!
//! | Variable | Default | |
//! |---|---|---|
//! | `DATABASE_URL` | (required) | `postgres://user:password@host/db` |
//! | `REDIS_URL` | `redis://127.0.0.1:6379` | |
//! | `LISTEN` | `0.0.0.0:8443` | |
//! | `TLS_CERT`, `TLS_KEY` | (none) | PEM files; without both, plain HTTP |
//! | `WORKERS` | every core | |
//! | `DB_POOL`, `CACHE_POOL` | `2`, `1` | connections per worker |
//! | `CACHE_TTL_SECS` | `60` | |
//! | `ACCESS_LOG` | `stdout` | or `off` |

use std::env;
use std::process::ExitCode;
use std::time::Duration;

use giong::net::TcpListener;
use giong_postgres::Config as DbConfig;
use giong_redis::Config as CacheConfig;
use giong_tls::TlsAcceptor;
use giong_web::Server;
use rest_api::{app, AccessLog, State, SCHEMA};

struct Settings {
    db: DbConfig,
    cache: CacheConfig,
    listen: String,
    tls: Option<TlsAcceptor>,
    workers: Option<usize>,
    db_pool: usize,
    cache_pool: usize,
    cache_ttl: Duration,
    access_log: bool,
}

fn var(name: &str) -> Option<String> {
    env::var(name).ok().filter(|v| !v.is_empty())
}

fn number<T: std::str::FromStr>(name: &str, default: T) -> Result<T, String> {
    match var(name) {
        None => Ok(default),
        Some(v) => v
            .parse()
            .map_err(|_| format!("{name}={v:?} is not a number")),
    }
}

fn settings() -> Result<Settings, String> {
    let url = var("DATABASE_URL")
        .ok_or("DATABASE_URL is not set; give postgres://user:password@host/db")?;
    let db = DbConfig::from_url(&url).map_err(|e| format!("DATABASE_URL: {e}"))?;
    let cache_url = var("REDIS_URL").unwrap_or_else(|| "redis://127.0.0.1:6379".into());
    let cache = CacheConfig::from_url(&cache_url).map_err(|e| format!("REDIS_URL: {e}"))?;
    let tls = match (var("TLS_CERT"), var("TLS_KEY")) {
        (Some(cert), Some(key)) => {
            let read = |path: &str| std::fs::read(path).map_err(|e| format!("{path}: {e}"));
            let acceptor = TlsAcceptor::from_pem(&read(&cert)?, &read(&key)?)
                .map_err(|e| format!("TLS_CERT/TLS_KEY: {e}"))?;
            Some(acceptor)
        }
        (None, None) => None,
        _ => return Err("set both TLS_CERT and TLS_KEY, or neither".into()),
    };
    let access_log = match var("ACCESS_LOG").as_deref() {
        None | Some("stdout") => true,
        Some("off") => false,
        Some(other) => return Err(format!("ACCESS_LOG={other:?}: use stdout or off")),
    };
    Ok(Settings {
        db,
        cache,
        listen: var("LISTEN").unwrap_or_else(|| "0.0.0.0:8443".into()),
        tls,
        workers: var("WORKERS").map(|_| number("WORKERS", 1)).transpose()?,
        db_pool: number("DB_POOL", 2)?,
        cache_pool: number("CACHE_POOL", 1)?,
        cache_ttl: Duration::from_secs(number("CACHE_TTL_SECS", 60)?),
        access_log,
    })
}

/// Creates the table, once, before the workers start.
fn migrate(db: &DbConfig) -> Result<(), String> {
    giong::block_on(async {
        let pool = giong_postgres::Pool::new(db.clone(), 1);
        pool.batch_execute(SCHEMA).await
    })
    .map_err(|e| format!("creating the table: {e}"))
}

fn main() -> ExitCode {
    let settings = match settings().and_then(|s| migrate(&s.db).map(|()| s)) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("rest-api: {e}");
            return ExitCode::FAILURE;
        }
    };
    if settings.tls.is_none() {
        eprintln!("rest-api: TLS_CERT and TLS_KEY are not set: serving plain HTTP");
    }
    let mut builder = giong::Builder::new()
        .shutdown_on_signals(true)
        .shutdown_grace(Duration::from_secs(10));
    if let Some(n) = settings.workers {
        builder = builder.workers(n);
    }
    let runtime = match builder.build() {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("rest-api: {e}");
            return ExitCode::FAILURE;
        }
    };
    let result = runtime.run(move |_worker| {
        // Everything a worker uses is its own: pools, app, listener.
        let state = State {
            db: giong_postgres::Pool::new(settings.db.clone(), settings.db_pool),
            cache: giong_redis::Pool::new(settings.cache.clone(), settings.cache_pool),
            cache_ttl: settings.cache_ttl,
        };
        let log = if settings.access_log {
            AccessLog::Stdout
        } else {
            AccessLog::Off
        };
        let app = app(state, log);
        let (listen, tls) = (settings.listen.clone(), settings.tls.clone());
        async move {
            let listener = TcpListener::bind(listen.as_str()).expect("bind LISTEN");
            // JSON bodies are small: refuse anything past 64 KiB.
            let server = Server::new().max_body_size(64 * 1024);
            match tls {
                Some(acceptor) => server.serve_tls(listener, acceptor, app).await,
                None => server.serve(listener, app).await,
            }
        }
    });
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("rest-api: {e}");
            ExitCode::FAILURE
        }
    }
}
