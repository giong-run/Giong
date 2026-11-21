//! Instruction counts per operation on giong's hot paths, for the CI gate in
//! `scripts/instr.sh` (see `bench/instr/README.md`).
//!
//!     bench-instr list
//!     valgrind --tool=callgrind bench-instr run <case> [iterations] [warmup]
//!
//! Each case sets up what it needs (a server, a connection), runs `warmup`
//! operations, then tells callgrind to zero its counters, runs `iterations`
//! operations and tells callgrind to dump its counters under the case's name.
//! Setup, handshakes and warm-up are therefore not counted, and the count
//! does not depend on how long anything took: everything runs on one thread
//! (`giong::Builder::block_on`), client and server alike, over loopback.
//!
//! The count includes both ends of a case: the client is giong code too
//! (a raw `TcpStream` where the case says "raw client", giong-http's client
//! otherwise), and the fake database servers are a few dozen instructions
//! per request.
//!
//! A run's memory comes from the size classes of `alloc.rs`, not from
//! glibc's malloc, whose cost depends on the state of the heap.
//!
//! Outside valgrind the client requests do nothing, so `run` also works as a
//! quick functional check (it prints the allocation counts either way).

mod alloc;
mod cases;
mod db;
mod vg;

use std::cell::Cell;

pub use alloc::{allocations, live_bytes};

#[global_allocator]
static GLOBAL: alloc::Counting = alloc::Counting::new();

/// The resident set of this process, in bytes.
pub fn rss() -> usize {
    let statm = std::fs::read_to_string("/proc/self/statm").unwrap_or_default();
    let pages: usize = statm
        .split_whitespace()
        .nth(1)
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    // SAFETY: sysconf has no preconditions.
    pages * unsafe { libc::sysconf(libc::_SC_PAGESIZE) } as usize
}

/// Runs a case's operations: `for _ in meter.iter() { ... }` yields
/// `warmup + iterations` times, zeroes callgrind's counters after the warm-up
/// and dumps them after the last iteration.
pub struct Meter {
    name: &'static str,
    iterations: u64,
    warmup: u64,
    allocs_at_start: Cell<u64>,
}

impl Meter {
    pub fn iter(&self) -> MeterIter<'_> {
        MeterIter { meter: self, i: 0 }
    }

    fn start(&self) {
        vg::zero_stats();
        self.allocs_at_start.set(allocations());
    }

    fn stop(&self) {
        let allocs = allocations() - self.allocs_at_start.get();
        vg::dump_stats_at(self.name);
        println!(
            "case={} iterations={} allocations_per_op={:.3}",
            self.name,
            self.iterations,
            allocs as f64 / self.iterations as f64
        );
    }
}

pub struct MeterIter<'a> {
    meter: &'a Meter,
    i: u64,
}

impl Iterator for MeterIter<'_> {
    type Item = u64;
    fn next(&mut self) -> Option<u64> {
        let m = self.meter;
        if self.i == m.warmup {
            m.start();
        }
        if self.i == m.warmup + m.iterations {
            m.stop();
            self.i += 1;
            return None;
        }
        if self.i > m.warmup + m.iterations {
            return None;
        }
        self.i += 1;
        Some(self.i - 1)
    }
}

fn usage() -> ! {
    eprintln!("usage: bench-instr list | info | run <case> [iterations] [warmup] | idle <case> [connections]");
    std::process::exit(2)
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("list") => {
            for case in cases::CASES {
                println!("{:<16} {}", case.name, case.what);
            }
            for case in cases::IDLE {
                println!("{:<16} {}", case.name, case.what);
            }
        }
        // The driver this process gets (GIONG_DRIVER) and its io_uring
        // features: recorded with the counts, which depend on them.
        Some("info") => {
            let choice = giong::Builder::new()
                .block_on(async { giong::driver_choice().to_string() })
                .expect("start the runtime");
            println!("{choice}");
        }
        Some("run") => {
            let Some(name) = args.get(1) else { usage() };
            let Some(case) = cases::CASES.iter().find(|c| c.name == name) else {
                eprintln!("unknown case {name}; `bench-instr list` lists them");
                std::process::exit(2)
            };
            let iterations = args.get(2).and_then(|v| v.parse().ok()).unwrap_or(2000);
            let warmup = args.get(3).and_then(|v| v.parse().ok()).unwrap_or(500);
            // What an allocation costs must not depend on the heap's
            // state (see `alloc.rs`).
            GLOBAL.fixed_cost();
            let meter = Meter {
                name: case.name,
                iterations,
                warmup,
                allocs_at_start: Cell::new(0),
            };
            giong::Builder::new()
                .block_on((case.run)(&meter))
                .expect("start the runtime");
        }
        Some("idle") => {
            let Some(name) = args.get(1) else { usage() };
            let Some(case) = cases::IDLE.iter().find(|c| c.name == name) else {
                eprintln!("unknown case {name}; `bench-instr list` lists them");
                std::process::exit(2)
            };
            let conns = args.get(2).and_then(|v| v.parse().ok()).unwrap_or(2000);
            giong::Builder::new()
                .block_on((case.run)(case.name, conns))
                .expect("start the runtime");
        }
        _ => usage(),
    }
}
