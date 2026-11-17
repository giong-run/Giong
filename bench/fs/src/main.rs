//! File I/O: giong against Tokio (`tokio::fs` and `spawn_blocking`, which
//! run on a pool of blocking threads), the io_uring runtimes monoio and
//! compio, and plain blocking `std`.
//!
//!   bench-fs <giong|tokio|monoio|compio|std> <small-files|random-read> <tasks> <operations> <dir>
//!
//! * `small-files`: read a whole 4 KiB file (open, stat, read, close).
//! * `random-read`: read 4 KiB at a random offset of one open 256 MiB file.
//!
//! Everything is in the page cache, so this measures the runtime's overhead
//! per operation, not the disk. Prints operations per second.
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::time::Instant;

const BLOCK: usize = 4096;
const BIG: u64 = 256 * 1024 * 1024;
const SMALL_FILES: usize = 1000;

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn offset(&mut self) -> u64 {
        (self.next() % (BIG / BLOCK as u64)) * BLOCK as u64
    }
}

fn small(dir: &Path, i: usize) -> PathBuf {
    dir.join(format!("small-{}", i % SMALL_FILES))
}

fn prepare(dir: &Path) {
    std::fs::create_dir_all(dir).unwrap();
    let big = dir.join("big");
    if std::fs::metadata(&big).map(|m| m.len()).unwrap_or(0) != BIG {
        let chunk: Vec<u8> = (0..1 << 20).map(|i| (i % 251) as u8).collect();
        let mut data = Vec::with_capacity(BIG as usize);
        while (data.len() as u64) < BIG {
            data.extend_from_slice(&chunk);
        }
        std::fs::write(&big, data).unwrap();
    }
    for i in 0..SMALL_FILES {
        let path = small(dir, i);
        if !path.exists() {
            std::fs::write(path, vec![i as u8; BLOCK]).unwrap();
        }
    }
    // Warm the page cache.
    std::fs::read(&big).unwrap();
    for i in 0..SMALL_FILES {
        std::fs::read(small(dir, i)).unwrap();
    }
}

fn run_std(mode: &str, ops: usize, dir: &Path) {
    use std::os::unix::fs::FileExt;
    match mode {
        "small-files" => {
            for i in 0..ops {
                assert_eq!(std::fs::read(small(dir, i)).unwrap().len(), BLOCK);
            }
        }
        _ => {
            let file = std::fs::File::open(dir.join("big")).unwrap();
            let mut rng = Rng(42);
            let mut buf = vec![0u8; BLOCK];
            for _ in 0..ops {
                assert_eq!(file.read_at(&mut buf, rng.offset()).unwrap(), BLOCK);
            }
        }
    }
}

fn run_giong(mode: &'static str, tasks: usize, ops: usize, dir: PathBuf) {
    giong::block_on(async move {
        let big = Rc::new(giong::fs::File::open(dir.join("big")).await.unwrap());
        let dir = Rc::new(dir);
        let handles: Vec<_> = (0..tasks)
            .map(|t| {
                let (big, dir) = (big.clone(), dir.clone());
                giong::spawn(async move {
                    let mut rng = Rng(42 + t as u64);
                    let mut buf = Vec::with_capacity(BLOCK);
                    for i in 0..ops / tasks {
                        if mode == "small-files" {
                            let path = small(&dir, t * 7919 + i);
                            assert_eq!(giong::fs::read(path).await.unwrap().len(), BLOCK);
                        } else {
                            let (res, b) = big.read_at(buf, rng.offset()).await;
                            assert_eq!(res.unwrap(), BLOCK);
                            buf = b;
                        }
                    }
                })
            })
            .collect();
        for handle in handles {
            handle.await.unwrap();
        }
    });
}

fn run_tokio(mode: &'static str, tasks: usize, ops: usize, dir: PathBuf) {
    use std::os::unix::fs::FileExt;
    let rt = tokio::runtime::Builder::new_current_thread().build().unwrap();
    let local = tokio::task::LocalSet::new();
    local.block_on(&rt, async move {
        let big = Arc::new(std::fs::File::open(dir.join("big")).unwrap());
        let dir = Rc::new(dir);
        let handles: Vec<_> = (0..tasks)
            .map(|t| {
                let (big, dir) = (big.clone(), dir.clone());
                tokio::task::spawn_local(async move {
                    let mut rng = Rng(42 + t as u64);
                    let mut buf = vec![0u8; BLOCK];
                    for i in 0..ops / tasks {
                        if mode == "small-files" {
                            let path = small(&dir, t * 7919 + i);
                            assert_eq!(tokio::fs::read(path).await.unwrap().len(), BLOCK);
                        } else {
                            // The idiomatic positional read on Tokio.
                            let (file, offset) = (big.clone(), rng.offset());
                            buf = tokio::task::spawn_blocking(move || {
                                assert_eq!(file.read_at(&mut buf, offset).unwrap(), BLOCK);
                                buf
                            })
                            .await
                            .unwrap();
                        }
                    }
                })
            })
            .collect();
        for handle in handles {
            handle.await.unwrap();
        }
    });
}

fn run_monoio(mode: &'static str, tasks: usize, ops: usize, dir: PathBuf) {
    let mut rt = monoio::RuntimeBuilder::<monoio::IoUringDriver>::new()
        .build()
        .unwrap();
    rt.block_on(async move {
        let big = Rc::new(monoio::fs::File::open(dir.join("big")).await.unwrap());
        let dir = Rc::new(dir);
        let handles: Vec<_> = (0..tasks)
            .map(|t| {
                let (big, dir) = (big.clone(), dir.clone());
                monoio::spawn(async move {
                    let mut rng = Rng(42 + t as u64);
                    let mut buf = Vec::with_capacity(BLOCK);
                    for i in 0..ops / tasks {
                        if mode == "small-files" {
                            let path = small(&dir, t * 7919 + i);
                            assert_eq!(monoio::fs::read(path).await.unwrap().len(), BLOCK);
                        } else {
                            let (res, b) = big.read_at(buf, rng.offset()).await;
                            assert_eq!(res.unwrap(), BLOCK);
                            buf = b;
                            buf.clear();
                        }
                    }
                })
            })
            .collect();
        for handle in handles {
            handle.await;
        }
    });
}

fn run_compio(mode: &'static str, tasks: usize, ops: usize, dir: PathBuf) {
    use compio::io::AsyncReadAt;
    compio::runtime::Runtime::new().unwrap().block_on(async move {
        let big = Rc::new(compio::fs::File::open(dir.join("big")).await.unwrap());
        let dir = Rc::new(dir);
        let handles: Vec<_> = (0..tasks)
            .map(|t| {
                let (big, dir) = (big.clone(), dir.clone());
                compio::runtime::spawn(async move {
                    let mut rng = Rng(42 + t as u64);
                    let mut buf = Vec::with_capacity(BLOCK);
                    for i in 0..ops / tasks {
                        if mode == "small-files" {
                            let path = small(&dir, t * 7919 + i);
                            assert_eq!(compio::fs::read(path).await.unwrap().len(), BLOCK);
                        } else {
                            let compio::BufResult(res, b) = big.read_at(buf, rng.offset()).await;
                            assert_eq!(res.unwrap(), BLOCK);
                            buf = b;
                            buf.clear();
                        }
                    }
                })
            })
            .collect();
        for handle in handles {
            handle.await.unwrap();
        }
    });
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() != 6 {
        eprintln!("usage: bench-fs <giong|tokio|monoio|compio|std> <small-files|random-read> <tasks> <operations> <dir>");
        std::process::exit(2);
    }
    let mode: &'static str = match args[2].as_str() {
        "small-files" => "small-files",
        "random-read" => "random-read",
        other => panic!("unknown workload {other}"),
    };
    let tasks: usize = args[3].parse().unwrap();
    let ops: usize = args[4].parse().unwrap();
    let dir = PathBuf::from(&args[5]);
    prepare(&dir);
    let start = Instant::now();
    match args[1].as_str() {
        "giong" => run_giong(mode, tasks, ops, dir),
        "tokio" => run_tokio(mode, tasks, ops, dir),
        "monoio" => run_monoio(mode, tasks, ops, dir),
        "compio" => run_compio(mode, tasks, ops, dir),
        "std" => run_std(mode, ops, &dir),
        other => panic!("unknown runtime {other}"),
    }
    println!("{:.0}", ops as f64 / start.elapsed().as_secs_f64());
}
