//! Core-primitive micro-benchmarks: the same workloads on Gióng and on the
//! other Rust runtimes, each with its own scheduler, timers and the channels
//! it ships (or the ones its users reach for when it ships none).
//!
//! `bench-core <runtime> <workload>` prints one CSV line:
//! `runtime,workload,ops,wall_ns,cpu_ns`. bench/core.sh runs the matrix.

use std::future::Future;
use std::time::{Duration, Instant};

fn cpu_ns() -> u64 {
    // SAFETY: getrusage only writes into the struct we pass.
    let mut u: libc::rusage = unsafe { std::mem::zeroed() };
    unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut u) };
    let t = |tv: libc::timeval| tv.tv_sec as u64 * 1_000_000_000 + tv.tv_usec as u64 * 1000;
    t(u.ru_utime) + t(u.ru_stime)
}

/// A yield that every runtime handles the same way: wake, then pending.
async fn plain_yield() {
    let mut done = false;
    std::future::poll_fn(|cx| {
        if done {
            std::task::Poll::Ready(())
        } else {
            done = true;
            cx.waker().wake_by_ref();
            std::task::Poll::Pending
        }
    })
    .await
}

const SPAWN: u64 = 400_000;
const YIELD_TASKS: u64 = 64;
const YIELDS: u64 = 20_000;
const PINGPONG: u64 = 400_000;
const MSGS: u64 = 4_000_000;
const ONESHOT: u64 = 400_000;
const TIMER_TASKS: u64 = 100_000;
const TIMEOUTS: u64 = 1_000_000;
const XMSGS: u64 = 4_000_000;
const CHAIN: u64 = 1_000;
const CHAIN_ROUNDS: u64 = 400;
const RR: u64 = 100_000;
const CONNECTS: u64 = 10_000;
const LOOKUPS: u64 = 200_000;

/// The workloads, written once and expanded inside each runtime's module,
/// where `spawn`, `yield_now`, the channels and the timers resolve to that
/// runtime's own.
macro_rules! workloads {
    () => {
        #[allow(unused_mut, clippy::redundant_closure)]
        pub async fn workload(name: &str) -> u64 {
            match name {
                // Spawn tiny tasks in batches of 1000 and join them all.
                "spawn" => {
                    let mut sum = 0u64;
                    for _ in 0..SPAWN / 1000 {
                        let mut handles = Vec::with_capacity(1000);
                        for i in 0..1000u64 {
                            handles.push(spawn(async move { i }));
                        }
                        for h in handles {
                            sum += h.await;
                        }
                    }
                    assert!(sum > 0);
                    SPAWN
                }
                // Many runnable tasks, each yielding: the cost of one poll.
                "yield" => {
                    let mut handles = Vec::new();
                    for _ in 0..YIELD_TASKS {
                        handles.push(spawn(async {
                            for _ in 0..YIELDS {
                                yield_now().await;
                            }
                            0u64
                        }));
                    }
                    for h in handles {
                        h.await;
                    }
                    YIELD_TASKS * YIELDS
                }
                // Two tasks bounce a value over two unbounded channels.
                "pingpong" => {
                    let (tx1, mut rx1) = unbounded();
                    let (tx2, mut rx2) = unbounded();
                    let peer = spawn(async move {
                        while let Some(v) = recv(&mut rx1).await {
                            send(&tx2, v + 1);
                        }
                        0u64
                    });
                    let mut v = 0;
                    for _ in 0..PINGPONG {
                        send(&tx1, v);
                        v = recv(&mut rx2).await.unwrap();
                    }
                    drop(tx1);
                    peer.await;
                    assert_eq!(v, PINGPONG);
                    PINGPONG
                }
                // One producer task streams values into an unbounded channel.
                "mpsc" => {
                    let (tx, mut rx) = unbounded();
                    let prod = spawn(async move {
                        for i in 0..MSGS {
                            send(&tx, i);
                            if i % 64 == 63 {
                                yield_now().await;
                            }
                        }
                        0u64
                    });
                    let mut n = 0;
                    while let Some(_) = recv(&mut rx).await {
                        n += 1;
                    }
                    prod.await;
                    assert_eq!(n, MSGS);
                    MSGS
                }
                // Four producers into a bounded channel of 128.
                "bounded" => {
                    let (tx, mut rx) = bounded(128);
                    let mut prods = Vec::new();
                    for _ in 0..4 {
                        let tx = tx.clone();
                        prods.push(spawn(async move {
                            for i in 0..MSGS / 4 {
                                bsend(&tx, i).await;
                            }
                            0u64
                        }));
                    }
                    drop(tx);
                    let mut n = 0;
                    while let Some(_) = brecv(&mut rx).await {
                        n += 1;
                    }
                    for p in prods {
                        p.await;
                    }
                    assert_eq!(n, MSGS);
                    MSGS
                }
                // A request/response pair: a oneshot answered by a new task.
                "oneshot" => {
                    let mut sum = 0;
                    for i in 0..ONESHOT {
                        let (tx, rx) = oneshot();
                        let t = spawn(async move {
                            osend(tx, i);
                            0u64
                        });
                        sum += orecv(rx).await;
                        t.await;
                    }
                    assert!(sum > 0);
                    ONESHOT
                }
                // Many tasks each sleep once (1-16 ms): timer wheel cost.
                "timers" => {
                    let mut handles = Vec::with_capacity(TIMER_TASKS as usize);
                    for i in 0..TIMER_TASKS {
                        handles.push(spawn(async move {
                            sleep(Duration::from_millis(1 + i % 16)).await;
                            0u64
                        }));
                    }
                    for h in handles {
                        h.await;
                    }
                    TIMER_TASKS
                }
                // A deadline on work that finishes at once: arm and cancel.
                "timeout" => {
                    let mut n = 0;
                    for i in 0..TIMEOUTS {
                        n += timeout_ready(i).await;
                    }
                    assert!(n > 0);
                    TIMEOUTS
                }
                // A chain of 1000 tasks passing a token along oneshots.
                "chain" => {
                    for _ in 0..CHAIN_ROUNDS {
                        let (first_tx, mut rx) = oneshot();
                        let mut tasks = Vec::with_capacity(CHAIN as usize);
                        for _ in 0..CHAIN {
                            let (tx, next) = oneshot();
                            let prev = std::mem::replace(&mut rx, next);
                            tasks.push(spawn(async move {
                                let v = orecv(prev).await;
                                osend(tx, v + 1);
                                0u64
                            }));
                        }
                        osend(first_tx, 0);
                        assert_eq!(orecv(rx).await, CHAIN);
                        for t in tasks {
                            t.await;
                        }
                    }
                    CHAIN * CHAIN_ROUNDS
                }
                // A plain thread outside the runtime feeds a task.
                "xthread" => {
                    let (tx, mut rx) = xchan();
                    let th = std::thread::spawn(move || {
                        for i in 0..XMSGS {
                            xsend(&tx, i);
                        }
                    });
                    let mut n = 0;
                    while let Some(_) = xrecv(&mut rx).await {
                        n += 1;
                    }
                    th.join().unwrap();
                    assert_eq!(n, XMSGS);
                    XMSGS
                }
                // One byte back and forth over a loopback TCP connection, both
                // ends on this thread: the runtime's cost per I/O operation.
                "tcp-rr" => tcp_rr().await,
                // Connect, accept and close loopback TCP connections.
                "connect" => tcp_connect().await,
                // Resolve "localhost" (from /etc/hosts).
                "lookup" => lookup().await,
                other => panic!("unknown workload {other}"),
            }
        }
    };
}

mod giong_rt {
    use super::*;
    use giong::sync::{mpsc, oneshot as os};
    pub fn block_on<F: Future>(f: F) -> F::Output {
        giong::block_on(f)
    }
    fn spawn<T: 'static>(f: impl Future<Output = T> + 'static) -> impl Future<Output = T> {
        let h = giong::spawn(f);
        async move { h.await.unwrap() }
    }
    use giong::yield_now;
    fn unbounded() -> (mpsc::UnboundedSender<u64>, mpsc::UnboundedReceiver<u64>) {
        mpsc::unbounded()
    }
    fn send(tx: &mpsc::UnboundedSender<u64>, v: u64) {
        tx.send(v).ok().unwrap();
    }
    async fn recv(rx: &mut mpsc::UnboundedReceiver<u64>) -> Option<u64> {
        rx.recv().await
    }
    fn bounded(n: usize) -> (mpsc::Sender<u64>, mpsc::Receiver<u64>) {
        mpsc::bounded(n)
    }
    async fn bsend(tx: &mpsc::Sender<u64>, v: u64) {
        tx.send(v).await.ok().unwrap();
    }
    async fn brecv(rx: &mut mpsc::Receiver<u64>) -> Option<u64> {
        rx.recv().await
    }
    fn oneshot() -> (os::Sender<u64>, os::Receiver<u64>) {
        os::channel()
    }
    fn osend(tx: os::Sender<u64>, v: u64) {
        tx.send(v).ok().unwrap();
    }
    async fn orecv(rx: os::Receiver<u64>) -> u64 {
        rx.await.unwrap()
    }
    async fn sleep(d: Duration) {
        giong::time::sleep(d).await
    }
    async fn timeout_ready(i: u64) -> u64 {
        giong::time::timeout(Duration::from_secs(10), std::future::ready(i | 1))
            .await
            .unwrap()
    }
    use self::{recv as xrecv, send as xsend, unbounded as xchan};
    async fn tcp_rr() -> u64 {
        use giong::net::{TcpListener, TcpStream};
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = l.local_addr().unwrap();
        let srv = spawn(async move {
            let s = l.accept().await.unwrap();
            s.set_nodelay(true).unwrap();
            let mut b = vec![0u8; 1];
            loop {
                let (r, bb) = s.read(b).await;
                if r.unwrap() == 0 {
                    break;
                }
                let (r, bb) = s.write_all(bb).await;
                r.unwrap();
                b = bb;
            }
            0u64
        });
        let c = TcpStream::connect(addr).await.unwrap();
        c.set_nodelay(true).unwrap();
        let mut b = vec![1u8; 1];
        for _ in 0..RR {
            let (r, bb) = c.write_all(b).await;
            r.unwrap();
            let (r, bb) = c.read(bb).await;
            assert_eq!(r.unwrap(), 1);
            b = bb;
        }
        drop(c);
        srv.await;
        RR
    }
    async fn tcp_connect() -> u64 {
        use giong::net::{TcpListener, TcpStream};
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = l.local_addr().unwrap();
        let srv = spawn(async move {
            for _ in 0..CONNECTS {
                drop(l.accept().await.unwrap());
            }
            0u64
        });
        for _ in 0..CONNECTS {
            drop(TcpStream::connect(addr).await.unwrap());
        }
        srv.await;
        CONNECTS
    }
    async fn lookup() -> u64 {
        for _ in 0..LOOKUPS {
            let a = giong::net::lookup_host("localhost:80").await.unwrap();
            std::hint::black_box(a);
        }
        LOOKUPS
    }
    workloads!();
}

/// Tokio's channels, shared by both Tokio flavours below.
macro_rules! tokio_prims {
    () => {
        use tokio::sync::{mpsc, oneshot as os};
        fn unbounded() -> (mpsc::UnboundedSender<u64>, mpsc::UnboundedReceiver<u64>) {
            mpsc::unbounded_channel()
        }
        fn send(tx: &mpsc::UnboundedSender<u64>, v: u64) {
            tx.send(v).ok().unwrap();
        }
        async fn recv(rx: &mut mpsc::UnboundedReceiver<u64>) -> Option<u64> {
            rx.recv().await
        }
        fn bounded(n: usize) -> (mpsc::Sender<u64>, mpsc::Receiver<u64>) {
            mpsc::channel(n)
        }
        async fn bsend(tx: &mpsc::Sender<u64>, v: u64) {
            tx.send(v).await.ok().unwrap();
        }
        async fn brecv(rx: &mut mpsc::Receiver<u64>) -> Option<u64> {
            rx.recv().await
        }
        fn oneshot() -> (os::Sender<u64>, os::Receiver<u64>) {
            os::channel()
        }
        fn osend(tx: os::Sender<u64>, v: u64) {
            tx.send(v).ok().unwrap();
        }
        async fn orecv(rx: os::Receiver<u64>) -> u64 {
            rx.await.unwrap()
        }
        async fn sleep(d: Duration) {
            tokio::time::sleep(d).await
        }
        async fn timeout_ready(i: u64) -> u64 {
            tokio::time::timeout(Duration::from_secs(10), std::future::ready(i | 1))
                .await
                .unwrap()
        }
        use self::{recv as xrecv, send as xsend, unbounded as xchan};
        use tokio::task::yield_now;
        async fn tcp_rr() -> u64 {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            use tokio::net::{TcpListener, TcpStream};
            let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = l.local_addr().unwrap();
            let srv = spawn(async move {
                let (mut s, _) = l.accept().await.unwrap();
                s.set_nodelay(true).unwrap();
                let mut b = [0u8; 1];
                loop {
                    if s.read(&mut b).await.unwrap() == 0 {
                        break;
                    }
                    s.write_all(&b).await.unwrap();
                }
                0u64
            });
            let mut c = TcpStream::connect(addr).await.unwrap();
            c.set_nodelay(true).unwrap();
            let mut b = [1u8; 1];
            for _ in 0..RR {
                c.write_all(&b).await.unwrap();
                assert_eq!(c.read(&mut b).await.unwrap(), 1);
            }
            drop(c);
            srv.await;
            RR
        }
        async fn tcp_connect() -> u64 {
            use tokio::net::{TcpListener, TcpStream};
            let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = l.local_addr().unwrap();
            let srv = spawn(async move {
                for _ in 0..CONNECTS {
                    drop(l.accept().await.unwrap());
                }
                0u64
            });
            for _ in 0..CONNECTS {
                drop(TcpStream::connect(addr).await.unwrap());
            }
            srv.await;
            CONNECTS
        }
        async fn lookup() -> u64 {
            for _ in 0..LOOKUPS {
                let a: Vec<_> = tokio::net::lookup_host("localhost:80")
                    .await
                    .unwrap()
                    .collect();
                std::hint::black_box(a);
            }
            LOOKUPS
        }
    };
}

/// Tokio's current-thread runtime with spawn (Send tasks): its fastest
/// single-thread setup.
mod tokio_ct {
    use super::*;
    pub fn block_on<F: Future>(f: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(f)
    }
    fn spawn<T: Send + 'static>(
        f: impl Future<Output = T> + Send + 'static,
    ) -> impl Future<Output = T> {
        let h = tokio::spawn(f);
        async move { h.await.unwrap() }
    }
    tokio_prims!();
    workloads!();
}

/// Tokio's current-thread runtime with a LocalSet (!Send tasks, like Gióng).
mod tokio_local {
    use super::*;
    pub fn block_on<F: Future>(f: F) -> F::Output {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        tokio::task::LocalSet::new().block_on(&rt, f)
    }
    fn spawn<T: 'static>(f: impl Future<Output = T> + 'static) -> impl Future<Output = T> {
        let h = tokio::task::spawn_local(f);
        async move { h.await.unwrap() }
    }
    tokio_prims!();
    workloads!();
}

/// Tokio's default multi-thread runtime, all cores (what `#[tokio::main]` gives).
mod tokio_mt {
    use super::*;
    pub fn block_on<F: Future>(f: F) -> F::Output {
        tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(f)
    }
    fn spawn<T: Send + 'static>(
        f: impl Future<Output = T> + Send + 'static,
    ) -> impl Future<Output = T> {
        let h = tokio::spawn(f);
        async move { h.await.unwrap() }
    }
    tokio_prims!();
    workloads!();
}

/// Channels from the async-channel crate (what smol re-exports) and
/// futures oneshots, for runtimes that ship none of their own.
macro_rules! generic_chans {
    () => {
        fn unbounded() -> (async_channel::Sender<u64>, async_channel::Receiver<u64>) {
            async_channel::unbounded()
        }
        fn send(tx: &async_channel::Sender<u64>, v: u64) {
            tx.try_send(v).ok().unwrap();
        }
        async fn recv(rx: &mut async_channel::Receiver<u64>) -> Option<u64> {
            rx.recv().await.ok()
        }
        fn bounded(n: usize) -> (async_channel::Sender<u64>, async_channel::Receiver<u64>) {
            async_channel::bounded(n)
        }
        async fn bsend(tx: &async_channel::Sender<u64>, v: u64) {
            tx.send(v).await.ok().unwrap();
        }
        async fn brecv(rx: &mut async_channel::Receiver<u64>) -> Option<u64> {
            rx.recv().await.ok()
        }
        fn oneshot() -> (
            futures_channel::oneshot::Sender<u64>,
            futures_channel::oneshot::Receiver<u64>,
        ) {
            futures_channel::oneshot::channel()
        }
        fn osend(tx: futures_channel::oneshot::Sender<u64>, v: u64) {
            tx.send(v).ok().unwrap();
        }
        async fn orecv(rx: futures_channel::oneshot::Receiver<u64>) -> u64 {
            rx.await.unwrap()
        }
    };
}

/// Cross-thread channel for runtimes without a Send channel of their own.
macro_rules! generic_xchan {
    () => {
        fn xchan() -> (async_channel::Sender<u64>, async_channel::Receiver<u64>) {
            async_channel::unbounded()
        }
        fn xsend(tx: &async_channel::Sender<u64>, v: u64) {
            tx.try_send(v).ok().unwrap();
        }
        async fn xrecv(rx: &mut async_channel::Receiver<u64>) -> Option<u64> {
            rx.recv().await.ok()
        }
    };
}

/// monoio with its io_uring driver; channels from local-sync, the !Send
/// channel crate by monoio's authors.
mod monoio_rt {
    use super::*;
    use local_sync::{mpsc, oneshot as os};
    pub fn block_on<F: Future>(f: F) -> F::Output {
        monoio::RuntimeBuilder::<monoio::IoUringDriver>::new()
            .enable_timer()
            .build()
            .unwrap()
            .block_on(f)
    }
    fn spawn<T: 'static>(f: impl Future<Output = T> + 'static) -> impl Future<Output = T> {
        monoio::spawn(f)
    }
    async fn yield_now() {
        plain_yield().await
    }
    fn unbounded() -> (mpsc::unbounded::Tx<u64>, mpsc::unbounded::Rx<u64>) {
        mpsc::unbounded::channel()
    }
    fn send(tx: &mpsc::unbounded::Tx<u64>, v: u64) {
        tx.send(v).ok().unwrap();
    }
    async fn recv(rx: &mut mpsc::unbounded::Rx<u64>) -> Option<u64> {
        rx.recv().await
    }
    fn bounded(n: usize) -> (mpsc::bounded::Tx<u64>, mpsc::bounded::Rx<u64>) {
        mpsc::bounded::channel(n)
    }
    async fn bsend(tx: &mpsc::bounded::Tx<u64>, v: u64) {
        tx.send(v).await.ok().unwrap();
    }
    async fn brecv(rx: &mut mpsc::bounded::Rx<u64>) -> Option<u64> {
        rx.recv().await
    }
    fn oneshot() -> (os::Sender<u64>, os::Receiver<u64>) {
        os::channel()
    }
    fn osend(tx: os::Sender<u64>, v: u64) {
        tx.send(v).ok().unwrap();
    }
    async fn orecv(rx: os::Receiver<u64>) -> u64 {
        rx.await.unwrap()
    }
    async fn sleep(d: Duration) {
        monoio::time::sleep(d).await
    }
    async fn timeout_ready(i: u64) -> u64 {
        monoio::time::timeout(Duration::from_secs(10), std::future::ready(i | 1))
            .await
            .unwrap()
    }
    generic_xchan!();
    async fn tcp_rr() -> u64 {
        use monoio::io::{AsyncReadRent, AsyncWriteRentExt};
        use monoio::net::{TcpListener, TcpStream};
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = l.local_addr().unwrap();
        let srv = spawn(async move {
            let (mut s, _) = l.accept().await.unwrap();
            s.set_nodelay(true).unwrap();
            let mut b = vec![0u8; 1];
            loop {
                let (r, bb) = s.read(b).await;
                if r.unwrap() == 0 {
                    break;
                }
                let (r, bb) = s.write_all(bb).await;
                r.unwrap();
                b = bb;
            }
            0u64
        });
        let mut c = TcpStream::connect(addr).await.unwrap();
        c.set_nodelay(true).unwrap();
        let mut b = vec![1u8; 1];
        for _ in 0..RR {
            let (r, bb) = c.write_all(b).await;
            r.unwrap();
            let (r, bb) = c.read(bb).await;
            assert_eq!(r.unwrap(), 1);
            b = bb;
        }
        drop(c);
        srv.await;
        RR
    }
    async fn tcp_connect() -> u64 {
        use monoio::net::{TcpListener, TcpStream};
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = l.local_addr().unwrap();
        let srv = spawn(async move {
            for _ in 0..CONNECTS {
                drop(l.accept().await.unwrap());
            }
            0u64
        });
        for _ in 0..CONNECTS {
            drop(TcpStream::connect(addr).await.unwrap());
        }
        srv.await;
        CONNECTS
    }
    async fn lookup() -> u64 {
        unimplemented!("monoio has no resolver")
    }
    workloads!();
}

/// glommio with its own local channels.
mod glommio_rt {
    use super::*;
    use glommio::channels::local_channel as lc;
    pub fn run(w: String) -> u64 {
        glommio::LocalExecutorBuilder::default()
            .spawn(move || async move { workload(&w).await })
            .unwrap()
            .join()
            .unwrap()
    }
    fn spawn<T: 'static>(f: impl Future<Output = T> + 'static) -> impl Future<Output = T> {
        glommio::spawn_local(f)
    }
    async fn yield_now() {
        plain_yield().await
    }
    fn unbounded() -> (lc::LocalSender<u64>, lc::LocalReceiver<u64>) {
        lc::new_unbounded()
    }
    fn send(tx: &lc::LocalSender<u64>, v: u64) {
        tx.try_send(v).ok().unwrap();
    }
    async fn recv(rx: &mut lc::LocalReceiver<u64>) -> Option<u64> {
        rx.recv().await
    }
    // glommio's local senders are not Clone: four producers share one Rc.
    fn bounded(n: usize) -> (std::rc::Rc<lc::LocalSender<u64>>, lc::LocalReceiver<u64>) {
        let (tx, rx) = lc::new_bounded(n);
        (std::rc::Rc::new(tx), rx)
    }
    async fn bsend(tx: &std::rc::Rc<lc::LocalSender<u64>>, v: u64) {
        tx.send(v).await.ok().unwrap();
    }
    async fn brecv(rx: &mut lc::LocalReceiver<u64>) -> Option<u64> {
        rx.recv().await
    }
    fn oneshot() -> (
        futures_channel::oneshot::Sender<u64>,
        futures_channel::oneshot::Receiver<u64>,
    ) {
        futures_channel::oneshot::channel()
    }
    fn osend(tx: futures_channel::oneshot::Sender<u64>, v: u64) {
        tx.send(v).ok().unwrap();
    }
    async fn orecv(rx: futures_channel::oneshot::Receiver<u64>) -> u64 {
        rx.await.unwrap()
    }
    async fn sleep(d: Duration) {
        glommio::timer::sleep(d).await
    }
    async fn timeout_ready(i: u64) -> u64 {
        glommio::timer::timeout(Duration::from_secs(10), async move { Ok(i | 1) })
            .await
            .unwrap()
    }
    generic_xchan!();
    async fn tcp_rr() -> u64 {
        use futures_lite::{AsyncReadExt, AsyncWriteExt};
        use glommio::net::{TcpListener, TcpStream};
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = l.local_addr().unwrap();
        let srv = spawn(async move {
            let mut s = l.accept().await.unwrap();
            s.set_nodelay(true).unwrap();
            let mut b = [0u8; 1];
            loop {
                if s.read(&mut b).await.unwrap() == 0 {
                    break;
                }
                s.write_all(&b).await.unwrap();
            }
            0u64
        });
        let mut c = TcpStream::connect(addr).await.unwrap();
        c.set_nodelay(true).unwrap();
        let mut b = [1u8; 1];
        for _ in 0..RR {
            c.write_all(&b).await.unwrap();
            assert_eq!(c.read(&mut b).await.unwrap(), 1);
        }
        drop(c);
        srv.await;
        RR
    }
    async fn tcp_connect() -> u64 {
        use glommio::net::{TcpListener, TcpStream};
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = l.local_addr().unwrap();
        let srv = spawn(async move {
            for _ in 0..CONNECTS {
                drop(l.accept().await.unwrap());
            }
            0u64
        });
        for _ in 0..CONNECTS {
            drop(TcpStream::connect(addr).await.unwrap());
        }
        srv.await;
        CONNECTS
    }
    async fn lookup() -> u64 {
        unimplemented!("glommio has no resolver")
    }
    workloads!();
}

/// compio with its io_uring driver; it ships no channels.
mod compio_rt {
    use super::*;
    pub fn block_on<F: Future>(f: F) -> F::Output {
        compio::runtime::Runtime::new().unwrap().block_on(f)
    }
    fn spawn<T: 'static>(f: impl Future<Output = T> + 'static) -> impl Future<Output = T> {
        let h = compio::runtime::spawn(f);
        async move { h.await.unwrap() }
    }
    async fn yield_now() {
        plain_yield().await
    }
    generic_chans!();
    async fn sleep(d: Duration) {
        compio::time::sleep(d).await
    }
    async fn timeout_ready(i: u64) -> u64 {
        compio::time::timeout(Duration::from_secs(10), std::future::ready(i | 1))
            .await
            .unwrap()
    }
    generic_xchan!();
    async fn tcp_rr() -> u64 {
        use compio::io::AsyncWriteExt;
        use compio::net::{TcpListener, TcpStream};
        let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        let srv = spawn(async move {
            let (mut s, _) = l.accept().await.unwrap();
            s.set_nodelay(true).unwrap();
            let mut b = vec![0u8; 1];
            loop {
                let compio::BufResult(r, bb) = compio::io::AsyncRead::read(&mut s, b).await;
                if r.unwrap() == 0 {
                    break;
                }
                let compio::BufResult(r, bb) = s.write_all(bb).await;
                r.unwrap();
                b = bb;
            }
            0u64
        });
        let mut c = TcpStream::connect(addr).await.unwrap();
        c.set_nodelay(true).unwrap();
        let mut b = vec![1u8; 1];
        for _ in 0..RR {
            let compio::BufResult(r, bb) = c.write_all(b).await;
            r.unwrap();
            let compio::BufResult(r, bb) = compio::io::AsyncRead::read(&mut c, bb).await;
            assert_eq!(r.unwrap(), 1);
            b = bb;
        }
        drop(c);
        srv.await;
        RR
    }
    async fn tcp_connect() -> u64 {
        use compio::net::{TcpListener, TcpStream};
        let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        let srv = spawn(async move {
            for _ in 0..CONNECTS {
                drop(l.accept().await.unwrap());
            }
            0u64
        });
        for _ in 0..CONNECTS {
            drop(TcpStream::connect(addr).await.unwrap());
        }
        srv.await;
        CONNECTS
    }
    async fn lookup() -> u64 {
        unimplemented!("compio: not measured")
    }
    workloads!();
}

/// smol: a LocalExecutor driven by async-io.
mod smol_rt {
    use super::*;
    thread_local! {
        static EX: &'static smol::LocalExecutor<'static> =
            Box::leak(Box::new(smol::LocalExecutor::new()));
    }
    pub fn block_on<F: Future>(f: F) -> F::Output {
        let ex = EX.with(|e| *e);
        smol::block_on(ex.run(f))
    }
    fn spawn<T: 'static>(f: impl Future<Output = T> + 'static) -> impl Future<Output = T> {
        EX.with(|e| e.spawn(f))
    }
    async fn yield_now() {
        smol::future::yield_now().await
    }
    generic_chans!();
    async fn sleep(d: Duration) {
        smol::Timer::after(d).await;
    }
    async fn timeout_ready(i: u64) -> u64 {
        use smol::future::FutureExt;
        // The timer is made before the race, as the other runtimes arm theirs.
        let timer = smol::Timer::after(Duration::from_secs(10));
        async move { Some(i | 1) }
            .or(async move {
                timer.await;
                None
            })
            .await
            .unwrap()
    }
    generic_xchan!();
    async fn tcp_rr() -> u64 {
        use smol::io::{AsyncReadExt, AsyncWriteExt};
        use smol::net::{TcpListener, TcpStream};
        let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        let srv = spawn(async move {
            let (mut s, _) = l.accept().await.unwrap();
            s.set_nodelay(true).unwrap();
            let mut b = [0u8; 1];
            loop {
                if s.read(&mut b).await.unwrap() == 0 {
                    break;
                }
                s.write_all(&b).await.unwrap();
            }
            0u64
        });
        let mut c = TcpStream::connect(addr).await.unwrap();
        c.set_nodelay(true).unwrap();
        let mut b = [1u8; 1];
        for _ in 0..RR {
            c.write_all(&b).await.unwrap();
            assert_eq!(c.read(&mut b).await.unwrap(), 1);
        }
        drop(c);
        srv.await;
        RR
    }
    async fn tcp_connect() -> u64 {
        use smol::net::{TcpListener, TcpStream};
        let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        let srv = spawn(async move {
            for _ in 0..CONNECTS {
                drop(l.accept().await.unwrap());
            }
            0u64
        });
        for _ in 0..CONNECTS {
            drop(TcpStream::connect(addr).await.unwrap());
        }
        srv.await;
        CONNECTS
    }
    async fn lookup() -> u64 {
        for _ in 0..LOOKUPS {
            let a = smol::net::resolve("localhost:80").await.unwrap();
            std::hint::black_box(a);
        }
        LOOKUPS
    }
    workloads!();
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let (rt, wl) = (args[1].clone(), args[2].clone());
    let start = Instant::now();
    let cpu0 = cpu_ns();
    let w = wl.clone();
    let ops = match rt.as_str() {
        "giong" => giong_rt::block_on(async move { giong_rt::workload(&w).await }),
        "tokio-ct" => tokio_ct::block_on(async move { tokio_ct::workload(&w).await }),
        "tokio-local" => tokio_local::block_on(async move { tokio_local::workload(&w).await }),
        "tokio-mt" => tokio_mt::block_on(async move { tokio_mt::workload(&w).await }),
        "monoio" => monoio_rt::block_on(async move { monoio_rt::workload(&w).await }),
        "glommio" => glommio_rt::run(w),
        "compio" => compio_rt::block_on(async move { compio_rt::workload(&w).await }),
        "smol" => smol_rt::block_on(async move { smol_rt::workload(&w).await }),
        other => panic!("unknown runtime {other}"),
    };
    let wall = start.elapsed().as_nanos() as u64;
    let cpu = cpu_ns() - cpu0;
    println!("{rt},{wl},{ops},{wall},{cpu}");
}
