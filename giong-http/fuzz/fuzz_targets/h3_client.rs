//! The HTTP/3 client against a server made of the input: a giong-quic
//! server over loopback that answers some requests honestly and breaks
//! the protocol in between (frames that do not belong, lengths that lie,
//! raw bytes, streams of its own, resets, closed connections; see
//! `giong-http/src/client/h3_hostile.rs`).
//!
//! Checks that the client never panics, that every request ends (a hang
//! fails the input), and that a response it accepts is one the server
//! meant, whole.
#![no_main]

use std::sync::{mpsc, Arc, Mutex, OnceLock};
use std::time::Duration;

use giong_http::client::Client;
use giong_http::fuzzing::h3_client::{ask, serve, Data, Source};
use giong_quic::{ClientConfig, Endpoint, ServerConfig};
use libfuzzer_sys::fuzz_target;

struct Job {
    data: Vec<u8>,
    done: mpsc::Sender<()>,
}

fn tls() -> (rustls::ServerConfig, rustls::ClientConfig) {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let cert = rcgen::generate_simple_self_signed(vec!["127.0.0.1".into()]).unwrap();
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

/// A server and a client of their own for `data`, and a few requests, one
/// after another and at once.
async fn play(server: &ServerConfig, client: &ClientConfig, data: &[u8]) {
    let endpoint = Endpoint::server("127.0.0.1:0".parse().unwrap(), server.clone()).unwrap();
    let url = format!("https://{}/", endpoint.local_addr().unwrap());
    let client = Client::builder()
        .http3_prior_knowledge(client.clone())
        .connect_timeout(Duration::from_secs(4))
        .timeout(Duration::from_secs(4))
        .build();
    let mut src = Data::new(data);
    let mut fork = src.fork();
    let serving = giong::spawn(async move {
        while let Some(conn) = endpoint.accept().await {
            drop(giong::spawn(serve(conn, fork.fork())));
        }
    });
    for _ in 0..2 {
        let mut tasks = Vec::new();
        for _ in 0..1 + src.below(3) {
            let (client, url) = (client.clone(), url.clone());
            tasks.push(giong::spawn(async move { ask(&client, &url).await }));
        }
        for task in tasks {
            assert!(task.await.is_ok(), "a request panicked");
        }
    }
    drop(client);
    serving.abort();
}

/// One runtime for the whole fuzzing process.
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
                    let server = ServerConfig::new(server_tls).unwrap();
                    let client = ClientConfig::new(client_tls).unwrap();
                    while let Some(job) = rx.recv().await {
                        let played = giong::time::timeout(
                            // Under libFuzzer's own timeout of 10 s, so that the
                            // failure says what hangs.
                            Duration::from_secs(9),
                            play(&server, &client, &job.data),
                        )
                        .await;
                        assert!(played.is_ok(), "a request never ends");
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
        data: data.to_vec(),
        done,
    };
    if runtime().lock().unwrap().send(job).is_err() {
        panic!("the runtime thread died");
    }
    result.recv().expect("the runtime thread died");
});
