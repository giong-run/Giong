//! Load generator for bench/postgres.sh: tokio-postgres on a current-thread
//! Tokio runtime (one core, like one giong worker).
//!
//!   bench-postgres-tokio <url> <point|list> <pool|client> <tasks> <seconds>
//!
//! `pool`: deadpool-postgres with POOL_SIZE connections (4 by default) and
//! prepare_cached, the usual way to run tokio-postgres in a service.
//! `client`: one tokio_postgres::Client shared by every task (it pipelines
//! too), with the statement prepared once.
//! Prints `ops=<per second> p50_us=<..> p99_us=<..> cpu=<%>` for the measured part,
//! after a warm-up of 2 seconds.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::{Duration, Instant, SystemTime};

use deadpool_postgres::{Manager, ManagerConfig, Pool, RecyclingMethod};
use tokio_postgres::{Client, NoTls, Row, Statement};

const LIST: &str = "SELECT id, name, email, created_at FROM bench_users WHERE id > $1 ORDER BY id LIMIT 100";
const POINT: &str = "SELECT id, name, email, created_at FROM bench_users WHERE id = $1";

enum Target {
    Pool(Pool),
    Client(Client, Statement),
}

impl Target {
    async fn query(&self, sql: &str, id: i64) -> Vec<Row> {
        match self {
            Target::Pool(p) => {
                let client = p.get().await.unwrap();
                let stmt = client.prepare_cached(sql).await.unwrap();
                client.query(&stmt, &[&id]).await.unwrap()
            }
            Target::Client(c, stmt) => c.query(stmt, &[&id]).await.unwrap(),
        }
    }
}

/// CPU time this process has used, in clock ticks (user + system).
fn cpu_ticks() -> u64 {
    let stat = std::fs::read_to_string("/proc/self/stat").unwrap();
    let fields: Vec<&str> = stat.rsplit(')').next().unwrap().split_whitespace().collect();
    fields[11].parse::<u64>().unwrap() + fields[12].parse::<u64>().unwrap()
}

fn decode(rows: &[Row]) -> usize {
    let mut n = 0;
    for row in rows {
        let id: i64 = row.get(0);
        let name: &str = row.get(1);
        let email: &str = row.get(2);
        let at: SystemTime = row.get(3);
        n += id as usize + name.len() + email.len() + (at > SystemTime::UNIX_EPOCH) as usize;
    }
    n
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let url = args[1].clone();
    let sql = if args[2] == "list" { LIST } else { POINT };
    let mode = args[3].clone();
    let tasks: usize = args[4].parse().unwrap();
    let secs: u64 = args[5].parse().unwrap();

    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
    let local = tokio::task::LocalSet::new();
    local.block_on(&rt, async move {
        let pg: tokio_postgres::Config = url.parse().unwrap();
        let target = Rc::new(match mode.as_str() {
            "pool" => {
                let mgr = Manager::from_config(pg, NoTls, ManagerConfig { recycling_method: RecyclingMethod::Fast });
                Target::Pool(Pool::builder(mgr).max_size(std::env::var("POOL_SIZE").map_or(4, |v| v.parse().unwrap())).build().unwrap())
            }
            _ => {
                let (client, conn) = pg.connect(NoTls).await.unwrap();
                tokio::task::spawn_local(conn);
                let stmt = client.prepare(sql).await.unwrap();
                Target::Client(client, stmt)
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
                        let rows = target.query(sql, id).await;
                        sink.set(sink.get() + decode(&rows));
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
