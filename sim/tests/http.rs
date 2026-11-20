//! giong-http server and client over plain TCP, TLS and HTTP/2, under
//! faults: a response must never belong to another request, every run must
//! end, and benign faults must never show as errors.

use std::rc::Rc;
use std::sync::OnceLock;
use std::time::Duration;

use giong::net::TcpListener;
use giong_http::client::Client;
use giong_http::{Request, Response, Server};
use giong_sim::Faults;
use giong_tls::{TlsAcceptor, TlsConnector};

#[derive(Clone, Copy, Debug, PartialEq)]
enum Mode {
    Plain,
    Tls,
    H2,
}

/// The server's certificate: the same in every process, whichever seed runs
/// first, because it is made in a run of its own with a fixed seed. (Made
/// inside the first run of a sweep, it depended on that run's seed, and a
/// seed that failed in a sweep did not fail when replayed alone.) Call it
/// before the first run: runs do not nest.
fn certificate() -> &'static (String, String) {
    static CERT: OnceLock<(String, String)> = OnceLock::new();
    CERT.get_or_init(|| {
        giong_sim::Sim::new(0)
            .faults(Faults::none())
            .run(|| {
                let key =
                    rcgen::generate_simple_self_signed(vec!["localhost".to_string()]).unwrap();
                (key.cert.pem(), key.key_pair.serialize_pem())
            })
            .0
    })
}

async fn echo(req: Request) -> Response {
    let mut body = req.path().as_bytes().to_vec();
    body.push(b'|');
    body.extend_from_slice(req.body());
    Response::new(200).body(body)
}

/// The server's request timeout: the most a request may take to arrive.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// A server and 6 client tasks sending 15 requests each through one shared
/// client. Returns (ok, errors). `jumps`: the clock may jump forward (hostile
/// faults), which can use up the server's request timeout; its 408 is then
/// the right answer and counts as an error, but only when the request really
/// took that long on the client's clock.
fn workload(mode: Mode, jumps: bool) -> (usize, usize) {
    let (cert, key) = certificate().clone();
    giong::block_on(async move {
        let listener = TcpListener::bind("127.0.0.1:8443").unwrap();
        // The low-memory refusal reads the host's free memory, which must
        // not decide a simulated run.
        let server = Server::new()
            .min_free_memory(0)
            .request_timeout(REQUEST_TIMEOUT)
            .idle_timeout(Duration::from_secs(30))
            .http2(mode == Mode::H2);
        match mode {
            Mode::Plain => {
                giong::spawn(async move { server.serve(listener, echo).await });
            }
            Mode::Tls | Mode::H2 => {
                let acceptor = TlsAcceptor::from_pem(cert.as_bytes(), key.as_bytes()).unwrap();
                giong::spawn(async move { server.serve_tls(listener, acceptor, echo).await });
            }
        }
        let mut builder = Client::builder()
            .timeout(Duration::from_secs(20))
            .connect_timeout(Duration::from_secs(10))
            .max_connections_per_host(3);
        let base = match mode {
            Mode::Plain => "http://127.0.0.1:8443",
            _ => {
                builder = builder.tls(TlsConnector::with_root_pem(cert.as_bytes()).unwrap());
                "https://localhost:8443"
            }
        };
        if mode == Mode::H2 {
            builder = builder.http2(true);
        }
        let client = Rc::new(builder.build());
        let tasks: Vec<_> = (0..6)
            .map(|t| {
                let client = client.clone();
                giong::spawn(async move {
                    let (mut ok, mut err) = (0, 0);
                    for i in 0..15 {
                        let path = format!("/echo/{t}/{i}");
                        let body = vec![b'a' + (i % 26) as u8; 100 + i * 997];
                        let sent = std::time::Instant::now();
                        match client
                            .post(&format!("{base}{path}"))
                            .body(body.clone())
                            .send()
                            .await
                        {
                            Ok(resp) if resp.status() == 408 => {
                                let took = sent.elapsed();
                                assert!(
                                    jumps && took >= REQUEST_TIMEOUT,
                                    "408 for {path} after only {took:?}"
                                );
                                err += 1;
                            }
                            Ok(resp) => {
                                assert_eq!(resp.status(), 200);
                                let h2 = resp.version() == giong_http::Version::Http2;
                                assert_eq!(h2, mode == Mode::H2, "protocol");
                                let mut want = path.into_bytes();
                                want.push(b'|');
                                want.extend_from_slice(&body);
                                assert!(
                                    resp.body() == want,
                                    "a response went to the wrong request"
                                );
                                ok += 1;
                            }
                            Err(_) => err += 1,
                        }
                    }
                    (ok, err)
                })
            })
            .collect();
        let mut total = (0, 0);
        for t in tasks {
            let (ok, err) = t.await.unwrap();
            total.0 += ok;
            total.1 += err;
        }
        total
    })
}

fn benign(mode: Mode) {
    certificate();
    for r in giong_sim::sweep(0..20, Faults::benign(), |_| {
        assert_eq!(workload(mode, false), (90, 0), "{mode:?}");
    }) {
        assert!(r.steps > 0);
    }
}

fn hostile(mode: Mode) {
    certificate();
    let seeds = std::env::var("GIONG_SIM_SEEDS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(50u64);
    let mut errors = 0;
    let reports = giong_sim::sweep(0..seeds, Faults::hostile(), |_| {
        let (ok, err) = workload(mode, true);
        assert_eq!(ok + err, 90);
    });
    for r in &reports {
        errors += r.injected.get("tcp-reset").copied().unwrap_or(0);
    }
    eprintln!(
        "{mode:?}: {} hostile runs, {errors} injected resets",
        reports.len()
    );
}

#[test]
fn plain_benign() {
    benign(Mode::Plain)
}
#[test]
fn tls_benign() {
    benign(Mode::Tls)
}
#[test]
fn h2_benign() {
    benign(Mode::H2)
}
#[test]
fn plain_hostile() {
    hostile(Mode::Plain)
}
#[test]
fn tls_hostile() {
    hostile(Mode::Tls)
}
#[test]
fn h2_hostile() {
    hostile(Mode::H2)
}
