//! A whole HTTP/3 connection: a control stream, other unidirectional
//! streams and request streams built from the input (well delimited
//! frames with field sections a real encoder made, and frames that do not
//! belong, lengths that lie and raw bytes now and then), sent by a
//! giong-quic client over loopback to `Server::serve_h3`.
//!
//! Checks that the server never panics and that every request stream
//! ends (a hang fails the input), that handlers only see requests they can
//! trust, and that a response stream that ends without an error holds
//! interim responses, one final response and its body, and nothing else.
#![no_main]

use std::net::SocketAddr;
use std::sync::{mpsc, Arc, Mutex, OnceLock};
use std::time::Duration;

use giong_http::fuzzing::h3_connection::{server, Fuzzed};
use giong_http::fuzzing::{check_h3_response, h3_input, H3Input};
use giong_quic::{ClientConfig, Endpoint, ServerConfig};
use libfuzzer_sys::fuzz_target;

struct Job {
    input: H3Input,
    done: mpsc::Sender<()>,
}

fn tls() -> (rustls::ServerConfig, rustls::ClientConfig) {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let cert = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
    let der = cert.cert.der().clone();
    let key = rustls::pki_types::PrivatePkcs8KeyDer::from(cert.key_pair.serialize_der());
    let mut server = rustls::ServerConfig::builder_with_provider(provider.clone())
        .with_protocol_versions(&[&rustls::version::TLS13])
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(vec![der.clone()], key.into())
        .unwrap();
    server.alpn_protocols = vec![b"h3".to_vec()];
    let mut roots = rustls::RootCertStore::empty();
    roots.add(der).unwrap();
    let mut client = rustls::ClientConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS13])
        .unwrap()
        .with_root_certificates(roots)
        .with_no_client_auth();
    client.alpn_protocols = vec![b"h3".to_vec()];
    (server, client)
}

/// Plays `input` on a new connection and checks every response that
/// arrives whole.
async fn play(endpoint: &Endpoint, addr: SocketAddr, cfg: &ClientConfig, input: &H3Input) {
    let conn = endpoint.connect(addr, "localhost", cfg).await.unwrap();
    // Streams stay open until the responses are read: an error on any of
    // them only shows in what the server does next.
    let mut open = Vec::new();
    for bytes in input.control.iter().chain(&input.uni) {
        let Ok(mut stream) = conn.open_uni().await else {
            break;
        };
        let _ = stream.write_all(bytes).await;
        open.push(stream);
    }
    let mut responses = Vec::new();
    for request in &input.requests {
        let Ok((mut send, recv)) = conn.open_bi().await else {
            break;
        };
        let _ = send.write_all(&request.bytes).await;
        if request.finish {
            let _ = send.finish();
        } else {
            send.reset(0x10c);
        }
        responses.push(recv);
    }
    for mut recv in responses {
        if let Ok(output) = recv.read_to_end(1 << 20).await {
            check_h3_response(&output);
        }
    }
    conn.close(0x100, b"");
}

/// One runtime, one server and one client endpoint for the whole fuzzing
/// process; every input gets a connection of its own.
fn runtime() -> &'static Mutex<giong::sync::mpsc::UnboundedSender<Job>> {
    static JOBS: OnceLock<Mutex<giong::sync::mpsc::UnboundedSender<Job>>> = OnceLock::new();
    JOBS.get_or_init(|| {
        let (tx, mut rx) = giong::sync::mpsc::unbounded::<Job>();
        std::thread::spawn(move || {
            giong::Builder::new()
                .workers(1)
                .recv_buffers(64, giong::net::GRO_RECV_BUFFER)
                .block_on(async move {
                    let (server_tls, client_tls) = tls();
                    let endpoint = Endpoint::server(
                        "127.0.0.1:0".parse().unwrap(),
                        ServerConfig::new(server_tls).unwrap(),
                    )
                    .unwrap();
                    let addr = endpoint.local_addr().unwrap();
                    drop(giong::spawn(async move {
                        server().serve_h3(endpoint, Fuzzed).await
                    }));
                    let client = Endpoint::client("127.0.0.1:0".parse().unwrap()).unwrap();
                    let cfg = ClientConfig::new(client_tls).unwrap();
                    while let Some(job) = rx.recv().await {
                        let played = giong::time::timeout(
                            // Under libFuzzer's own timeout of 10 s, so that the
                            // failure says what hangs.
                            Duration::from_secs(5),
                            play(&client, addr, &cfg, &job.input),
                        )
                        .await;
                        assert!(played.is_ok(), "a request stream never ends");
                        let _ = job.done.send(());
                    }
                })
                .expect("runtime");
        });
        Mutex::new(tx)
    })
}

fuzz_target!(|data: &[u8]| {
    let (done, result) = mpsc::channel();
    let job = Job {
        input: h3_input(data),
        done,
    };
    if runtime().lock().unwrap().send(job).is_err() {
        panic!("the runtime thread died");
    }
    result.recv().expect("the runtime thread died");
});
