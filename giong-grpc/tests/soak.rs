//! Soak test for gRPC: a long run of calls of every kind with faults mixed
//! in, then a check that nothing piled up.
//!
//! Every worker serves the interop test service on one shared port and
//! runs clients against it. The clients mix unary calls (small, large,
//! compressed), long server streams, long bidirectional streams and client
//! streams with what goes wrong in production: calls cancelled halfway,
//! response streams dropped, deadlines that pass mid-stream, statuses with
//! errors, handlers that are still sending when their client is gone,
//! connections that stop in the middle of a message, bytes that are not
//! HTTP/2 at all, and channels thrown away and reopened. A thread outside
//! the runtime samples the process's descriptors and resident memory.
//!
//! At the end it checks that:
//! * every call ended the way its scenario says, and no other error
//!   showed up;
//! * no handler is still running on the server: each one ended or was
//!   dropped when its call was cancelled or its client went away;
//! * every worker's driver is back to idle (no tasks, nothing in flight);
//! * the descriptor count did not grow during the run and is back to
//!   where it started once the runtime is dropped;
//! * resident memory levelled off after the warm-up.
//!
//! It runs for `GIONG_SOAK_SECS` seconds (default 60), so it is not part
//! of the default run:
//!
//! ```text
//! GIONG_SOAK_SECS=3600 cargo test --release -p giong-grpc --test soak -- --ignored --nocapture
//! ```

mod common;

use std::cell::Cell;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use common::*;
use giong::net::{TcpListener, TcpStream};
use giong::time::{sleep, timeout};
use giong_grpc::{CallOptions, Channel, Code, Reply, Request, RequestStream, Sink, Status};
use giong_http::client::Client;

/// Client tasks per worker.
const CLIENTS: usize = 12;

#[derive(Default)]
struct Counters {
    unary: AtomicU64,
    streamed: AtomicU64,
    messages: AtomicU64,
    cancelled: AtomicU64,
    late: AtomicU64,
    failed_as_asked: AtomicU64,
    broken_connections: AtomicU64,
    channels: AtomicU64,
    /// Handlers running on the servers now.
    live: AtomicI64,
    /// Handlers started.
    handled: AtomicU64,
    /// Outcomes that none of the scenarios should produce.
    unexpected: AtomicU64,
}

impl Counters {
    fn bump(counter: &AtomicU64) {
        counter.fetch_add(1, Ordering::Relaxed);
    }

    fn unexpected(&self, what: &str, detail: impl std::fmt::Debug) {
        if self.unexpected.fetch_add(1, Ordering::Relaxed) < 20 {
            eprintln!("soak: unexpected outcome in {what}: {detail:?}");
        }
    }
}

/// xorshift64: each task has its own, seeded from its identity.
struct Rng(Cell<u64>);

impl Rng {
    fn new(seed: u64) -> Self {
        Self(Cell::new(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1))
    }

    fn below(&self, n: u64) -> u64 {
        let mut x = self.0.get();
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0.set(x);
        x % n
    }
}

/// The interop service, counting the handlers that are running.
struct Counted {
    inner: Interop,
    counters: Arc<Counters>,
}

struct Live(Arc<Counters>);

impl Drop for Live {
    fn drop(&mut self) {
        self.0.live.fetch_sub(1, Ordering::Relaxed);
    }
}

impl Counted {
    fn live(&self) -> Live {
        self.counters.live.fetch_add(1, Ordering::Relaxed);
        Counters::bump(&self.counters.handled);
        Live(self.counters.clone())
    }
}

impl TestService for Counted {
    async fn empty_call(
        &self,
        request: Request<Empty<'static>>,
    ) -> Result<Reply<Empty<'static>>, Status> {
        let _live = self.live();
        self.inner.empty_call(request).await
    }

    async fn unary_call(
        &self,
        request: Request<SimpleRequest<'static>>,
    ) -> Result<Reply<SimpleResponse<'static>>, Status> {
        let _live = self.live();
        self.inner.unary_call(request).await
    }

    async fn cacheable_unary_call(
        &self,
        request: Request<SimpleRequest<'static>>,
    ) -> Result<Reply<SimpleResponse<'static>>, Status> {
        let _live = self.live();
        self.inner.cacheable_unary_call(request).await
    }

    async fn streaming_output_call(
        &self,
        request: Request<StreamingOutputCallRequest<'static>>,
        responses: Sink<StreamingOutputCallResponse<'static>>,
    ) -> Result<(), Status> {
        let _live = self.live();
        self.inner.streaming_output_call(request, responses).await
    }

    async fn streaming_input_call(
        &self,
        requests: RequestStream<StreamingInputCallRequest<'static>>,
    ) -> Result<Reply<StreamingInputCallResponse<'static>>, Status> {
        let _live = self.live();
        self.inner.streaming_input_call(requests).await
    }

    async fn full_duplex_call(
        &self,
        requests: RequestStream<StreamingOutputCallRequest<'static>>,
        responses: Sink<StreamingOutputCallResponse<'static>>,
    ) -> Result<(), Status> {
        let _live = self.live();
        self.inner.full_duplex_call(requests, responses).await
    }

    async fn half_duplex_call(
        &self,
        requests: RequestStream<StreamingOutputCallRequest<'static>>,
        responses: Sink<StreamingOutputCallResponse<'static>>,
    ) -> Result<(), Status> {
        let _live = self.live();
        self.inner.half_duplex_call(requests, responses).await
    }

    async fn unimplemented_call(
        &self,
        request: Request<Empty<'static>>,
    ) -> Result<Reply<Empty<'static>>, Status> {
        let _live = self.live();
        self.inner.unimplemented_call(request).await
    }
}

fn payload(body: &[u8]) -> Payload<'_> {
    Payload {
        body,
        ..Default::default()
    }
}

fn sized(size: i32, interval_us: i32) -> ResponseParameters<'static> {
    ResponseParameters {
        size,
        interval_us,
        ..Default::default()
    }
}

/// One client: calls of every kind, one after the other, until the
/// deadline.
async fn client(addr: SocketAddr, deadline: Instant, rng: Rng, c: Arc<Counters>) {
    let base = format!("http://{addr}");
    let open = |c: &Counters| {
        Counters::bump(&c.channels);
        TestServiceClient::new(Channel::new(&base).expect("address"))
    };
    let mut service = open(&c);
    let mut calls = 0u64;
    while Instant::now() < deadline {
        calls += 1;
        // A new channel now and then: its connection closes, another opens.
        if calls % 200 == 0 {
            service = open(&c);
        }
        match rng.below(12) {
            // Unary, small or large, compressed or not, both ways.
            0..=2 => {
                let large = rng.below(8) == 0;
                let request_size = rng.below(if large { 300_000 } else { 2_000 }) as usize;
                let response_size = rng.below(if large { 300_000 } else { 2_000 }) as i32;
                let body = vec![rng.below(256) as u8; request_size];
                let compressed = BoolValue {
                    value: rng.below(2) == 0,
                    ..Default::default()
                };
                let request = SimpleRequest {
                    response_size,
                    payload: Some(&payload(&body)),
                    response_compressed: Some(&compressed),
                    ..Default::default()
                };
                let options = CallOptions::new().compress(rng.below(2) == 0);
                match service.unary_call(&request, options).await {
                    Ok(reply) => match reply.message() {
                        Ok(message)
                            if message.payload().map_or(0, |p| p.body().len())
                                == response_size as usize =>
                        {
                            Counters::bump(&c.unary);
                        }
                        other => c.unexpected("unary reply", other.map(|_| "wrong size")),
                    },
                    Err(status) => c.unexpected("unary", status),
                }
            }
            // Unary that the server answers with an error.
            3 => {
                let code = 1 + rng.below(16) as i32;
                let asked = EchoStatus {
                    code,
                    message: "as the soak asks \u{2603}",
                    ..Default::default()
                };
                let request = SimpleRequest {
                    response_status: Some(&asked),
                    ..Default::default()
                };
                match service.unary_call(&request, CallOptions::new()).await {
                    Err(status)
                        if u32::from(status.code().number()) == code as u32
                            && status.message() == "as the soak asks \u{2603}" =>
                    {
                        Counters::bump(&c.failed_as_asked);
                    }
                    other => c.unexpected("unary with a status", other.map(|_| "ok")),
                }
            }
            // A server stream read to its end.
            4 | 5 => {
                let count = 1 + rng.below(60) as usize;
                let parameters: Vec<_> = (0..count)
                    .map(|_| sized(rng.below(3_000) as i32, rng.below(300) as i32))
                    .collect();
                let request = StreamingOutputCallRequest {
                    response_parameters: &parameters,
                    ..Default::default()
                };
                let mut got = 0;
                let outcome = async {
                    let mut responses = service
                        .streaming_output_call(&request, CallOptions::new())
                        .await?;
                    while let Some(received) = responses.message().await? {
                        let size = received.message()?.payload().map_or(0, |p| p.body().len());
                        if size != parameters[got].size as usize {
                            return Err(Status::new(Code::DataLoss, "a response of another size"));
                        }
                        got += 1;
                    }
                    Ok(())
                }
                .await;
                match outcome {
                    Ok(()) if got == count => {
                        Counters::bump(&c.streamed);
                        c.messages.fetch_add(got as u64, Ordering::Relaxed);
                    }
                    other => c.unexpected("server stream", (got, count, other)),
                }
            }
            // A server stream dropped partway: the handler is still sending.
            6 => {
                let parameters: Vec<_> = (0..200).map(|_| sized(1_000, 200)).collect();
                let request = StreamingOutputCallRequest {
                    response_parameters: &parameters,
                    ..Default::default()
                };
                match service
                    .streaming_output_call(&request, CallOptions::new())
                    .await
                {
                    Ok(mut responses) => {
                        for _ in 0..rng.below(20) {
                            if let Err(status) = responses.message().await {
                                c.unexpected("server stream before the drop", status);
                                break;
                            }
                        }
                        drop(responses);
                        Counters::bump(&c.cancelled);
                    }
                    Err(status) => c.unexpected("server stream to drop", status),
                }
            }
            // A long conversation, then a clean end or a cancel.
            7 | 8 => {
                let rounds = 1 + rng.below(150);
                let cancel = rng.below(3) == 0;
                let (mut requests, mut responses) = service.full_duplex_call(CallOptions::new());
                let mut failed = false;
                for _ in 0..rounds {
                    let size = rng.below(2_000) as i32;
                    let body = vec![0u8; rng.below(2_000) as usize];
                    let parameters = [sized(size, 0)];
                    let message = StreamingOutputCallRequest {
                        response_parameters: &parameters,
                        payload: Some(&payload(&body)),
                        ..Default::default()
                    };
                    let outcome = async {
                        requests.send(&message).await?;
                        match responses.message().await? {
                            Some(received) => Ok(received.as_bytes().len()),
                            None => Err(Status::new(Code::DataLoss, "the stream ended early")),
                        }
                    }
                    .await;
                    match outcome {
                        Ok(bytes) if bytes >= size as usize => {
                            Counters::bump(&c.messages);
                        }
                        other => {
                            c.unexpected("conversation", other);
                            failed = true;
                            break;
                        }
                    }
                }
                if failed {
                    continue;
                }
                if cancel {
                    requests.cancel();
                    match responses.message().await {
                        Err(status) if status.code() == Code::Cancelled => {
                            Counters::bump(&c.cancelled);
                        }
                        other => c.unexpected("cancelled conversation", other.map(|_| "ok")),
                    }
                } else {
                    requests.finish();
                    match responses.message().await {
                        Ok(None) => Counters::bump(&c.streamed),
                        other => c.unexpected("end of conversation", other.map(|_| "message")),
                    }
                }
            }
            // A client stream.
            9 => {
                let mut call = service.streaming_input_call(CallOptions::new());
                let mut total = 0;
                let mut outcome = Ok(());
                for _ in 0..1 + rng.below(40) {
                    let body = vec![1u8; rng.below(4_000) as usize];
                    total += body.len();
                    let message = StreamingInputCallRequest {
                        payload: Some(&payload(&body)),
                        ..Default::default()
                    };
                    outcome = call.send(&message).await;
                    if outcome.is_err() {
                        break;
                    }
                }
                let outcome = match outcome {
                    Ok(()) => call.finish().await,
                    Err(status) => Err(status),
                };
                match outcome.as_ref().map(|reply| reply.message()) {
                    Ok(Ok(message)) if message.aggregated_payload_size() as usize == total => {
                        Counters::bump(&c.streamed);
                    }
                    _ => c.unexpected("client stream", outcome.map(|_| total)),
                }
            }
            // A deadline that passes while the server is still answering.
            10 => {
                let parameters = [sized(10, 0), sized(10, 200_000)];
                let request = StreamingOutputCallRequest {
                    response_parameters: &parameters,
                    ..Default::default()
                };
                let options = CallOptions::new().timeout(Duration::from_millis(30));
                let outcome = async {
                    let mut responses = service.streaming_output_call(&request, options).await?;
                    while responses.message().await?.is_some() {}
                    Ok::<_, Status>(())
                }
                .await;
                match outcome {
                    Err(status) if status.code() == Code::DeadlineExceeded => {
                        Counters::bump(&c.late);
                    }
                    other => c.unexpected("deadline", other),
                }
            }
            // A connection that breaks in the middle of things.
            _ => {
                broken_connection(addr, &rng, &c).await;
                Counters::bump(&c.broken_connections);
            }
        }
    }
}

/// Clients that vanish or talk nonsense: the server has to drop the calls
/// and the connection without a trace.
async fn broken_connection(addr: SocketAddr, rng: &Rng, c: &Counters) {
    match rng.below(3) {
        // A call whose request stops in the middle of a message; then the
        // client and its connection are gone.
        0 => {
            let client = Client::builder().http2_prior_knowledge(true).build();
            let (request, body) = client
                .post(&format!(
                    "http://{addr}/grpc.testing.TestService/FullDuplexCall"
                ))
                .header("content-type", "application/grpc")
                .header("te", "trailers")
                .streamed_body();
            let sending = giong::spawn(request.send_streaming());
            // A whole empty message, then five bytes of a 300-byte one.
            if let Err(e) = body
                .send(vec![0, 0, 0, 0, 0, 0, 0, 0, 1, 44, 1, 2, 3, 4, 5])
                .await
            {
                c.unexpected("half a message", e);
            }
            sleep(Duration::from_millis(rng.below(5))).await;
            if rng.below(2) == 0 {
                body.abort();
            }
            sending.abort();
            drop(client);
        }
        // Bytes that are not HTTP/2, or an HTTP/2 preface and then
        // nonsense: the connection is refused, the server goes on.
        kind => {
            let Ok(stream) = TcpStream::connect(addr).await else {
                c.unexpected("connect", "refused");
                return;
            };
            let mut bytes = if kind == 1 {
                b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n".to_vec()
            } else {
                Vec::new()
            };
            bytes.extend((0..rng.below(200)).map(|_| rng.below(256) as u8));
            let _ = stream.write_all(bytes).await;
            // Whatever the server answers, if anything, then the close.
            let _ = timeout(Duration::from_millis(rng.below(10)), stream.recv()).await;
        }
    }
}

fn open_fds() -> usize {
    std::fs::read_dir("/proc/self/fd").unwrap().count()
}

fn rss_kib() -> u64 {
    let status = std::fs::read_to_string("/proc/self/status").unwrap();
    let line = status.lines().find(|l| l.starts_with("VmRSS:")).unwrap();
    line.split_whitespace().nth(1).unwrap().parse().unwrap()
}

fn median(values: &mut [u64]) -> u64 {
    values.sort_unstable();
    values[values.len() / 2]
}

#[test]
#[ignore = "runs for GIONG_SOAK_SECS (default 60); run with --ignored"]
fn soak() {
    let secs: u64 = std::env::var("GIONG_SOAK_SECS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(60);
    let workers = std::thread::available_parallelism().map_or(2, |n| n.get().clamp(2, 4));
    let fds_before = open_fds();

    // A free port for every worker's listener to share (`TcpListener::bind`
    // sets SO_REUSEPORT).
    let addr = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap();

    let counters = Arc::new(Counters::default());
    let samples = Arc::new(Mutex::new(Vec::<(u64, u64)>::new()));
    let stop = Arc::new(AtomicBool::new(false));
    let sampler = {
        let (samples, stop, c) = (samples.clone(), stop.clone(), counters.clone());
        std::thread::spawn(move || {
            let start = Instant::now();
            while !stop.load(Ordering::Relaxed) {
                std::thread::sleep(Duration::from_secs(1));
                let (fds, rss) = (open_fds() as u64, rss_kib());
                samples.lock().unwrap().push((fds, rss));
                let t = start.elapsed().as_secs();
                if t % 10 == 0 {
                    eprintln!(
                        "soak: {t:>5}s  fds {fds:>4}  rss {rss:>6} KiB  unary {:>8}  streams {:>7}  \
                         messages {:>9}  cancelled {:>6}  late {:>6}  broken {:>6}  live {:>3}",
                        c.unary.load(Ordering::Relaxed),
                        c.streamed.load(Ordering::Relaxed),
                        c.messages.load(Ordering::Relaxed),
                        c.cancelled.load(Ordering::Relaxed),
                        c.late.load(Ordering::Relaxed),
                        c.broken_connections.load(Ordering::Relaxed),
                        c.live.load(Ordering::Relaxed),
                    );
                }
            }
        })
    };

    let deadline = Instant::now() + Duration::from_secs(secs);
    let idle = Arc::new(Mutex::new(Vec::new()));
    {
        let (counters, idle) = (counters.clone(), idle.clone());
        giong::Builder::new()
            .workers(workers)
            .build()
            .unwrap()
            .run(move |worker| {
                let (counters, idle) = (counters.clone(), idle.clone());
                async move {
                    let listener = TcpListener::bind(addr).expect("bind");
                    let service = Counted {
                        inner: Interop,
                        counters: counters.clone(),
                    };
                    let grpc = giong_grpc::Server::new().add(TestServiceServer::new(service));
                    // The server outlives the clients by a moment: calls
                    // started right at the deadline still get their answer.
                    let server = giong::spawn(async move {
                        let http = giong_http::Server::new();
                        let left = deadline.saturating_duration_since(Instant::now())
                            + Duration::from_secs(3);
                        let _ = timeout(left, http.serve(listener, grpc)).await;
                    });
                    // Let every worker's listener come up first.
                    sleep(Duration::from_millis(100)).await;

                    let mut tasks = Vec::new();
                    for i in 0..CLIENTS {
                        let rng = Rng::new((worker * CLIENTS + i) as u64 + 1);
                        tasks.push(giong::spawn(client(addr, deadline, rng, counters.clone())));
                    }
                    for task in tasks {
                        task.await.expect("task");
                    }
                    server.await.expect("server");

                    // Connections close and handlers see their clients go
                    // away; give the last of that a moment.
                    let settle = Instant::now() + Duration::from_secs(10);
                    loop {
                        sleep(Duration::from_millis(20)).await;
                        let stats = giong::observe::worker_stats();
                        // The main future is not counted as a task; the one
                        // operation left in flight is the eventfd read that
                        // cross-thread wake-ups complete.
                        let quiet = stats.tasks == 0
                            && stats.tracked_ops == 0
                            && stats.inflight <= 1
                            && stats.free_recv_buffers == stats.recv_buffers;
                        if quiet || Instant::now() > settle {
                            idle.lock()
                                .unwrap()
                                .push((worker, format!("{stats:?}"), quiet));
                            break;
                        }
                    }
                }
            })
            .unwrap();
    }
    stop.store(true, Ordering::Relaxed);
    sampler.join().unwrap();

    let c = &counters;
    let load = |counter: &AtomicU64| counter.load(Ordering::Relaxed);
    eprintln!(
        "soak: done after {secs}s on {workers} workers: {} unary calls, {} streams ended, \
         {} messages in streams, {} calls cancelled, {} deadlines passed, {} statuses as asked, \
         {} broken connections, {} channels, {} handlers run",
        load(&c.unary),
        load(&c.streamed),
        load(&c.messages),
        load(&c.cancelled),
        load(&c.late),
        load(&c.failed_as_asked),
        load(&c.broken_connections),
        load(&c.channels),
        load(&c.handled),
    );

    // 1. Every scenario behaved as designed, and every kind of it ran.
    assert_eq!(load(&c.unexpected), 0, "unexpected outcomes");
    for (what, counter) in [
        ("unary calls", &c.unary),
        ("streams", &c.streamed),
        ("cancelled calls", &c.cancelled),
        ("deadlines", &c.late),
        ("statuses", &c.failed_as_asked),
        ("broken connections", &c.broken_connections),
    ] {
        assert!(load(counter) > 0, "no {what} ran");
    }

    // 2. No handler outlived its call.
    assert_eq!(c.live.load(Ordering::Relaxed), 0, "handlers still running");

    // 3. Every worker's driver went back to idle.
    for (worker, stats, quiet) in idle.lock().unwrap().iter() {
        assert!(quiet, "worker {worker} did not go idle: {stats}");
    }

    // 4. Descriptors: nothing left once the runtime is gone, and no growth
    //    during the run beyond the connections that happen to be open.
    assert_eq!(open_fds(), fds_before, "descriptors leaked");
    let samples = samples.lock().unwrap();
    if samples.len() >= 8 {
        let half = samples.len() / 2;
        let first = samples[1..half].iter().map(|s| s.0).max().unwrap();
        let second = samples[half..samples.len() - 1]
            .iter()
            .map(|s| s.0)
            .max()
            .unwrap();
        // Each open connection holds two descriptors, client and server.
        let slack = (2 * workers * CLIENTS + 16) as u64;
        assert!(
            second <= first + slack,
            "descriptor count grew: at most {first} in the first half, {second} in the second"
        );

        // 5. Memory: the median of the last quarter is not far above the
        //    median of the second quarter, once caches and pools are warm.
        let quarter = samples.len() / 4;
        let mut early: Vec<u64> = samples[quarter..2 * quarter].iter().map(|s| s.1).collect();
        let mut late: Vec<u64> = samples[3 * quarter..].iter().map(|s| s.1).collect();
        let (early, late) = (median(&mut early), median(&mut late));
        eprintln!("soak: resident memory {early} KiB early, {late} KiB late");
        assert!(
            late <= early + early / 4 + 16 * 1024,
            "resident memory grew from {early} KiB to {late} KiB"
        );
    }
}
