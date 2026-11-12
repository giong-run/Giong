//! Load generator for bench/mysql.sh: mysql_async or SQLx on a current-thread
//! Tokio runtime (one core, like one giong worker).
//!
//!   bench-mysql-tokio <url> <point|list> <async|async-default|sqlx> <tasks> <seconds>
//!
//! Each runs through its own pool of POOL_SIZE connections (4 by default),
//! the usual way to use it in a service; both keep prepared statements per
//! connection. Neither pipelines: a connection runs one statement at a time.
//! `async-default` is mysql_async with its pool's default of resetting each
//! connection when it is returned.
//! `url` is `mysql://user:password@host:port/database` without parameters.
//! Prints `ops=<per second> p50_us=<..> p99_us=<..> cpu=<%>` for the measured
//! part, after a warm-up of 2 seconds.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::{Duration, Instant};

use mysql_async::prelude::Queryable;
use sqlx::Row as _;
use time::PrimitiveDateTime;

const LIST: &str = "SELECT id, name, email, created_at FROM bench_users WHERE id > ? ORDER BY id LIMIT 100";
const POINT: &str = "SELECT id, name, email, created_at FROM bench_users WHERE id = ?";

enum Target {
    Async(mysql_async::Pool),
    Sqlx(sqlx::MySqlPool),
}

impl Target {
    /// Runs the statement and decodes every column of every row.
    async fn query(&self, sql: &str, id: i64) -> usize {
        let mut n = 0;
        match self {
            Target::Async(p) => {
                let mut conn = p.get_conn().await.unwrap();
                let rows: Vec<(i64, String, String, PrimitiveDateTime)> =
                    conn.exec(sql, (id,)).await.unwrap();
                for (id, name, email, at) in rows {
                    n += id as usize + name.len() + email.len() + (at.year() > 1970) as usize;
                }
            }
            Target::Sqlx(p) => {
                let rows = sqlx::query(sql).bind(id).fetch_all(p).await.unwrap();
                for row in &rows {
                    let id: i64 = row.get(0);
                    let name: &str = row.get(1);
                    let email: &str = row.get(2);
                    let at: PrimitiveDateTime = row.get(3);
                    n += id as usize + name.len() + email.len() + (at.year() > 1970) as usize;
                }
            }
        }
        n
    }
}

/// CPU time this process has used, in clock ticks (user + system).
fn cpu_ticks() -> u64 {
    let stat = std::fs::read_to_string("/proc/self/stat").unwrap();
    let fields: Vec<&str> = stat.rsplit(')').next().unwrap().split_whitespace().collect();
    fields[11].parse::<u64>().unwrap() + fields[12].parse::<u64>().unwrap()
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let url = args[1].clone();
    let sql = if args[2] == "list" { LIST } else { POINT };
    let driver = args[3].clone();
    let tasks: usize = args[4].parse().unwrap();
    let secs: u64 = args[5].parse().unwrap();
    let size: usize = std::env::var("POOL_SIZE").map_or(4, |v| v.parse().unwrap());

    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
    let local = tokio::task::LocalSet::new();
    local.block_on(&rt, async move {
        let target = Rc::new(match driver.as_str() {
            "async" | "async-default" => {
                // By default mysql_async resets a connection when it goes back
                // to the pool, which costs a round trip and the connection's
                // prepared statements; `async` turns that off.
                let reset = driver == "async-default";
                let opts = mysql_async::OptsBuilder::from_opts(mysql_async::Opts::from_url(&url).unwrap())
                    .pool_opts(
                        mysql_async::PoolOpts::default()
                            .with_constraints(mysql_async::PoolConstraints::new(1, size).unwrap())
                            .with_reset_connection(reset),
                    );
                Target::Async(mysql_async::Pool::new(opts))
            }
            _ => {
                // SQLx would try TLS on its own (its default is PREFERRED).
                let pool = sqlx::mysql::MySqlPoolOptions::new()
                    .max_connections(size as u32)
                    .connect(&format!("{url}?ssl-mode=DISABLED"))
                    .await
                    .unwrap();
                Target::Sqlx(pool)
            }
        });
        let measuring = Rc::new(Cell::new(false));
        let stop = Rc::new(Cell::new(false));
        let samples = Rc::new(RefCell::new(Vec::<u32>::with_capacity(1 << 22)));
        let sink = Rc::new(Cell::new(0usize));
        let handles: Vec<_> = (0..tasks)
            .map(|t| {
                let (target, measuring, stop, samples, sink) =
                    (target.clone(), measuring.clone(), stop.clone(), samples.clone(), sink.clone());
                tokio::task::spawn_local(async move {
                    let mut id = (t as i64 * 7919) % 10_000;
                    while !stop.get() {
                        id = (id * 48271 + 11) % 9_900 + 1;
                        let start = Instant::now();
                        let n = target.query(sql, id).await;
                        sink.set(sink.get() + n);
                        if measuring.get() {
                            samples.borrow_mut().push(start.elapsed().as_micros() as u32);
                        }
                    }
                })
            })
            .collect();
        tokio::time::sleep(Duration::from_secs(2)).await;
        measuring.set(true);
        let ticks = cpu_ticks();
        let begin = Instant::now();
        tokio::time::sleep(Duration::from_secs(secs)).await;
        measuring.set(false);
        let elapsed = begin.elapsed().as_secs_f64();
        let hz: f64 = std::env::var("CLK_TCK").ok().and_then(|v| v.parse().ok()).unwrap_or(100.0);
        let cpu = (cpu_ticks() - ticks) as f64 * 100.0 / (hz * elapsed);
        stop.set(true);
        for h in handles {
            h.await.unwrap();
        }
        let mut s = samples.borrow_mut();
        s.sort_unstable();
        let pct = |p: f64| s[((s.len() as f64 * p) as usize).min(s.len() - 1)];
        println!(
            "ops={:.0} p50_us={} p99_us={} cpu={:.0} sink={}",
            s.len() as f64 / elapsed,
            pct(0.50),
            pct(0.99),
            cpu,
            sink.get() % 10
        );
    });
}
