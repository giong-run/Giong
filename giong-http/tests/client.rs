//! The HTTP client against giong-http's server and against scripted servers
//! that misbehave on purpose, on one giong worker.

use std::cell::{Cell, RefCell};
use std::collections::HashSet;
use std::future::Future;
use std::net::SocketAddr;
use std::rc::Rc;
use std::time::{Duration, Instant};

use giong::net::{TcpListener, TcpStream};
use giong::time::sleep;
use giong_http::client::{Client, ErrorKind};
use giong_http::{Request, Response, Server};
use giong_tls::{TlsAcceptor, TlsConnector};

// ------------------------------------------------------------------ harness

/// Starts giong-http's server on this worker; returns its base URL.
fn serve<H, Fut>(handler: H) -> String
where
    H: Fn(Request) -> Fut + 'static,
    Fut: Future<Output = Response> + 'static,
{
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    giong::spawn(async move { Server::new().serve(listener, handler).await });
    format!("http://{addr}")
}

/// Starts a server that runs `script` for each connection it accepts, with
/// the connection's number (from 0).
fn scripted<S, Fut>(script: S) -> String
where
    S: Fn(usize, TcpStream) -> Fut + 'static,
    Fut: Future<Output = ()> + 'static,
{
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let script = Rc::new(script);
    giong::spawn(async move {
        for n in 0.. {
            let Ok(stream) = listener.accept().await else {
                return;
            };
            giong::spawn((script.clone())(n, stream));
        }
    });
    format!("http://{addr}")
}

/// Reads one request (head and `Content-Length` body) from `buf` and the
/// stream; `None` when the client closed the connection first.
async fn read_request(stream: &TcpStream, buf: &mut Vec<u8>) -> Option<String> {
    loop {
        if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            let head = String::from_utf8(buf[..i + 4].to_vec()).unwrap();
            let len: usize = head
                .lines()
                .find_map(|l| {
                    let (n, v) = l.split_once(':')?;
                    n.eq_ignore_ascii_case("content-length")
                        .then(|| v.trim().parse().unwrap())
                })
                .unwrap_or(0);
            while buf.len() < i + 4 + len {
                let chunk = stream.recv().await.ok()?;
                if chunk.is_empty() {
                    return None;
                }
                buf.extend_from_slice(&chunk);
            }
            let request = String::from_utf8_lossy(&buf[..i + 4 + len]).into_owned();
            buf.drain(..i + 4 + len);
            return Some(request);
        }
        let chunk = stream.recv().await.ok()?;
        if chunk.is_empty() {
            return None;
        }
        buf.extend_from_slice(&chunk);
    }
}

async fn write(stream: &TcpStream, bytes: &[u8]) {
    let _ = stream.write_all(bytes.to_vec()).await;
}

/// A self-signed certificate for `localhost`, as (certificate PEM, key PEM).
fn certificate() -> (String, String) {
    let key = rcgen::generate_simple_self_signed(vec!["localhost".to_string()]).unwrap();
    (key.cert.pem(), key.key_pair.serialize_pem())
}

// -------------------------------------------------------------------- basics

#[test]
fn methods_headers_and_bodies_go_both_ways() {
    giong::block_on(async {
        let base = serve(|req: Request| async move {
            let id = req.header_str("x-id").unwrap_or("-").to_owned();
            let agent = req.header_str("user-agent").unwrap_or("-").to_owned();
            let kind = req.header_str("content-type").unwrap_or("-").to_owned();
            let text = format!(
                "{} {} id={id} agent={agent} type={kind} body={}",
                req.method(),
                req.target(),
                String::from_utf8_lossy(req.body())
            );
            Response::text(text).header("X-Answer", "42")
        });
        let client = Client::new();

        let r = client
            .get(&format!("{base}/a/b?c=1"))
            .header("X-Id", "7")
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
        assert!(r.is_success());
        assert_eq!(r.header("x-answer"), Some("42"));
        assert_eq!(r.header("Content-Type"), Some("text/plain; charset=utf-8"));
        let agent = concat!("giong-http/", env!("CARGO_PKG_VERSION"));
        assert_eq!(
            r.text().unwrap(),
            format!("GET /a/b?c=1 id=7 agent={agent} type=- body=")
        );
        assert_eq!(r.url(), format!("{base}/a/b?c=1"));

        let r = client
            .post(&format!("{base}/items"))
            .json(r#"{"name":"x"}"#)
            .send()
            .await
            .unwrap();
        assert_eq!(
            r.text().unwrap(),
            format!(r#"POST /items id=- agent={agent} type=application/json body={{"name":"x"}}"#)
        );

        for method in ["PUT", "PATCH", "DELETE", "OPTIONS"] {
            let r = client
                .request(method, &base)
                .header("user-agent", "mine/1")
                .body("x")
                .send()
                .await
                .unwrap();
            assert_eq!(
                r.text().unwrap(),
                format!("{method} / id=- agent=mine/1 type=- body=x")
            );
        }

        let r = client.head(&base).send().await.unwrap();
        assert_eq!(r.status(), 200);
        assert!(r.body().is_empty());
        assert!(r.header("content-length").is_some());
        // The connection is still in step after a HEAD.
        let r = client.get(&base).send().await.unwrap();
        assert!(r.text().unwrap().starts_with("GET / "));
        assert_eq!(client.connections(&base), (1, 1));
    });
}

#[test]
fn connections_are_kept_alive_and_shared_up_to_the_limit() {
    giong::block_on(async {
        let peers = Rc::new(RefCell::new(HashSet::<SocketAddr>::new()));
        let in_flight = Rc::new(Cell::new(0));
        let peak = Rc::new(Cell::new(0));
        let base = serve({
            let (peers, in_flight, peak) = (peers.clone(), in_flight.clone(), peak.clone());
            move |req: Request| {
                let (peers, in_flight, peak) = (peers.clone(), in_flight.clone(), peak.clone());
                async move {
                    peers.borrow_mut().insert(req.peer_addr().unwrap());
                    in_flight.set(in_flight.get() + 1);
                    peak.set(peak.get().max(in_flight.get()));
                    sleep(Duration::from_millis(20)).await;
                    in_flight.set(in_flight.get() - 1);
                    Response::text("ok")
                }
            }
        });

        // One after another: one connection.
        let client = Client::new();
        for _ in 0..10 {
            client.get(&base).send().await.unwrap();
        }
        assert_eq!(peers.borrow().len(), 1);
        assert_eq!(client.connections(&base), (1, 1));

        // Twenty at once, at most three connections.
        peers.borrow_mut().clear();
        let client = Client::builder().max_connections_per_host(3).build();
        let tasks: Vec<_> = (0..20)
            .map(|_| {
                let (client, url) = (client.clone(), base.clone());
                giong::spawn(async move { client.get(&url).send().await.map(|r| r.status()) })
            })
            .collect();
        for t in tasks {
            assert_eq!(t.await.unwrap().unwrap(), 200);
        }
        assert_eq!(peers.borrow().len(), 3);
        assert_eq!(peak.get(), 3);
        assert_eq!(client.connections(&base), (3, 3));
    });
}

#[test]
fn error_statuses_are_responses_until_asked() {
    giong::block_on(async {
        let base = serve(|req: Request| async move {
            match req.path() {
                "/missing" => Response::new(404).body(r#"{"error":"no such user"}"#),
                _ => Response::new(503),
            }
        });
        let client = Client::new();
        let r = client.get(&format!("{base}/missing")).send().await.unwrap();
        assert_eq!(r.status(), 404);
        let e = r.error_for_status().unwrap_err();
        assert_eq!(e.kind(), ErrorKind::Status);
        assert_eq!(e.status(), Some(404));
        assert!(!e.is_transient());
        assert!(
            e.to_string()
                .contains(r#"404 Not Found: "{\"error\":\"no such user\"}""#),
            "{e}"
        );
        let e = client
            .get(&base)
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap_err();
        assert!(e.is_transient());
        assert!(client
            .get(&base)
            .send()
            .await
            .unwrap()
            .text()
            .unwrap()
            .is_empty());
    });
}

// ---------------------------------------------- connections closing under it

#[test]
fn a_connection_the_server_closed_while_idle_is_not_used() {
    giong::block_on(async {
        let connections = Rc::new(Cell::new(0));
        let base = scripted({
            let connections = connections.clone();
            move |_, stream| {
                connections.set(connections.get() + 1);
                async move {
                    let mut buf = Vec::new();
                    read_request(&stream, &mut buf).await;
                    write(&stream, b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok").await;
                    // Keep-alive was promised, but the server closes soon
                    // after, as servers do when a connection is idle.
                    sleep(Duration::from_millis(20)).await;
                }
            }
        });
        let client = Client::new();
        for method in ["GET", "POST"] {
            let r = client.request(method, &base).send().await.unwrap();
            assert_eq!(r.text().unwrap(), "ok");
            sleep(Duration::from_millis(100)).await;
        }
        assert_eq!(connections.get(), 2);
    });
}

#[test]
fn a_kept_alive_connection_closing_as_the_request_goes_is_retried_only_when_safe() {
    giong::block_on(async {
        // The first connection answers one request, then reads the next one
        // and closes without an answer: the race a client cannot see coming.
        let base = scripted(|n, stream| async move {
            let mut buf = Vec::new();
            if n % 2 == 0 {
                read_request(&stream, &mut buf).await;
                write(
                    &stream,
                    b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nfirst",
                )
                .await;
                read_request(&stream, &mut buf).await;
            } else {
                read_request(&stream, &mut buf).await;
                write(
                    &stream,
                    b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nretry",
                )
                .await;
                read_request(&stream, &mut buf).await;
            }
        });
        let client = Client::new();
        assert_eq!(
            client.get(&base).send().await.unwrap().text().unwrap(),
            "first"
        );
        assert_eq!(
            client.get(&base).send().await.unwrap().text().unwrap(),
            "retry"
        );

        // Connection 2 answers once and then fails the next request.
        let client = Client::new();
        let base2 = scripted(|_, stream| async move {
            let mut buf = Vec::new();
            read_request(&stream, &mut buf).await;
            write(&stream, b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok").await;
            read_request(&stream, &mut buf).await;
        });
        client.post(&base2).send().await.unwrap();
        let e = client.post(&base2).send().await.unwrap_err();
        assert_eq!(e.kind(), ErrorKind::Closed);
        assert!(e.to_string().contains("not sent again by itself"), "{e}");
        assert!(e.is_transient());
        let _ = base;
    });
}

#[test]
fn a_dropped_request_closes_its_connection() {
    giong::block_on(async {
        let base = serve(|req: Request| async move {
            if req.path() == "/slow" {
                sleep(Duration::from_secs(5)).await;
            }
            Response::text("ok")
        });
        let client = Client::new();
        client.get(&base).send().await.unwrap();
        assert_eq!(client.connections(&base), (1, 1));
        let slow = client.get(&format!("{base}/slow")).send();
        assert!(giong::time::timeout(Duration::from_millis(50), slow)
            .await
            .is_err());
        assert_eq!(client.connections(&base), (0, 0));
        assert_eq!(
            client.get(&base).send().await.unwrap().text().unwrap(),
            "ok"
        );
    });
}

// ------------------------------------------------------------ framing rules

#[test]
fn chunked_close_delimited_and_informational_responses_are_read() {
    giong::block_on(async {
        let base = scripted(|_, stream| async move {
            let mut buf = Vec::new();
            while let Some(req) = read_request(&stream, &mut buf).await {
                let path = req.split(' ').nth(1).unwrap().to_owned();
                match path.as_str() {
                    "/chunked" => {
                        write(&stream, b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n5;ext=1\r\nhello\r\n").await;
                        sleep(Duration::from_millis(10)).await;
                        write(&stream, b"6\r\n world\r\n0\r\nX-Trailer: t\r\n\r\n").await;
                    }
                    "/hints" => {
                        write(&stream, b"HTTP/1.1 103 Early Hints\r\nLink: </a.css>\r\n\r\nHTTP/1.1 100 Continue\r\n\r\nHTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok").await;
                    }
                    "/no-content" => {
                        write(&stream, b"HTTP/1.1 204 No Content\r\n\r\n").await;
                    }
                    _ => {
                        write(&stream, b"HTTP/1.0 200 OK\r\n\r\nuntil the end").await;
                        return;
                    }
                }
            }
        });
        let client = Client::new();
        let r = client.get(&format!("{base}/chunked")).send().await.unwrap();
        assert_eq!(r.text().unwrap(), "hello world");
        let r = client.get(&format!("{base}/hints")).send().await.unwrap();
        assert_eq!((r.status(), r.text().unwrap()), (200, "ok"));
        assert_eq!(r.header("link"), None);
        let r = client
            .get(&format!("{base}/no-content"))
            .send()
            .await
            .unwrap();
        assert_eq!((r.status(), r.body().len()), (204, 0));
        assert_eq!(client.connections(&base), (1, 1));
        let r = client.get(&format!("{base}/close")).send().await.unwrap();
        assert_eq!(r.text().unwrap(), "until the end");
        assert_eq!(client.connections(&base), (0, 0));
    });
}

#[test]
fn malformed_responses_are_errors_and_their_connection_is_dropped() {
    giong::block_on(async {
        let base = scripted(|_, stream| async move {
            let mut buf = Vec::new();
            let Some(req) = read_request(&stream, &mut buf).await else {
                return;
            };
            let answer: &[u8] = match req.split(' ').nth(1).unwrap() {
                "/two-lengths" => {
                    b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nContent-Length: 3\r\n\r\nabc"
                }
                "/bare-lf" => b"HTTP/1.1 200 OK\nContent-Length: 2\n\nok",
                "/not-http" => b"SSH-2.0-OpenSSH_9.6\r\n\r\n",
                "/gzip" => b"HTTP/1.1 200 OK\r\nTransfer-Encoding: gzip, chunked\r\n\r\n",
                "/bad-chunk" => b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\nzz\r\n",
                "/switch" => b"HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\n\r\n",
                "/cut" => b"HTTP/1.1 200 OK\r\nContent-Length: 10\r\n\r\nshort",
                _ => b"",
            };
            write(&stream, answer).await;
            sleep(Duration::from_millis(50)).await;
        });
        let client = Client::new();
        for (path, kind, says) in [
            (
                "/two-lengths",
                ErrorKind::Protocol,
                "two different Content-Length",
            ),
            ("/bare-lf", ErrorKind::Protocol, "bare line feed"),
            ("/not-http", ErrorKind::Protocol, "not HTTP/1.1"),
            ("/gzip", ErrorKind::Protocol, "does not decode"),
            (
                "/bad-chunk",
                ErrorKind::Protocol,
                "chunked body is malformed",
            ),
            ("/switch", ErrorKind::Protocol, "switched protocols"),
            (
                "/cut",
                ErrorKind::Closed,
                "in the middle of the response body",
            ),
            ("/nothing", ErrorKind::Closed, "without answering"),
        ] {
            let e = client
                .get(&format!("{base}{path}"))
                .send()
                .await
                .unwrap_err();
            assert_eq!(e.kind(), kind, "{path}: {e}");
            assert!(e.to_string().contains(says), "{path}: {e}");
            assert!(
                e.to_string().contains(&format!("GET {base}{path}")),
                "{path}: {e}"
            );
            assert_eq!(client.connections(&base), (0, 0), "{path}");
        }
    });
}

#[test]
fn bytes_after_a_response_keep_its_connection_out_of_the_pool() {
    giong::block_on(async {
        let base = scripted(|_, stream| async move {
            let mut buf = Vec::new();
            while read_request(&stream, &mut buf).await.is_some() {
                write(&stream, b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nokHTTP/1.1 200 OK\r\nContent-Length: 6\r\n\r\nforged").await;
            }
        });
        let client = Client::new();
        for _ in 0..2 {
            assert_eq!(
                client.get(&base).send().await.unwrap().text().unwrap(),
                "ok"
            );
            assert_eq!(client.connections(&base), (0, 0));
        }
    });
}

// ------------------------------------------------------------------- limits

#[test]
fn responses_over_the_limits_fail() {
    giong::block_on(async {
        let base = scripted(|_, stream| async move {
            let mut buf = Vec::new();
            let Some(req) = read_request(&stream, &mut buf).await else {
                return;
            };
            let big = "x".repeat(2000);
            let answer = match req.split(' ').nth(1).unwrap() {
                "/length" => format!("HTTP/1.1 200 OK\r\nContent-Length: 2000\r\n\r\n{big}"),
                "/chunked" => format!("HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n7d0\r\n{big}\r\n0\r\n\r\n"),
                "/close" => format!("HTTP/1.0 200 OK\r\n\r\n{big}"),
                _ => format!("HTTP/1.1 200 OK\r\nX-Big: {big}\r\nContent-Length: 0\r\n\r\n"),
            };
            write(&stream, answer.as_bytes()).await;
        });
        let client = Client::builder()
            .max_response_size(1000)
            .max_head_size(1000)
            .build();
        for path in ["/length", "/chunked", "/close", "/head"] {
            let e = client
                .get(&format!("{base}{path}"))
                .send()
                .await
                .unwrap_err();
            assert_eq!(e.kind(), ErrorKind::TooLarge, "{path}: {e}");
            assert!(e.to_string().contains("ClientBuilder::max_"), "{path}: {e}");
        }
    });
}

#[test]
fn a_server_that_does_not_answer_hits_the_timeout() {
    giong::block_on(async {
        let base = scripted(|_, stream| async move {
            sleep(Duration::from_secs(10)).await;
            drop(stream);
        });
        let client = Client::builder()
            .timeout(Duration::from_millis(300))
            .build();
        let start = Instant::now();
        let e = client.get(&base).send().await.unwrap_err();
        assert_eq!(e.kind(), ErrorKind::Timeout);
        assert!(e.to_string().contains("did not finish within 300ms"), "{e}");
        assert!(start.elapsed() < Duration::from_secs(5));
        let e = client
            .get(&base)
            .timeout(Duration::from_millis(100))
            .send()
            .await
            .unwrap_err();
        assert!(e.to_string().contains("within 100ms"), "{e}");
        assert_eq!(client.connections(&base), (0, 0));

        // Nothing listening is a connect error, at once.
        // (A standard library listener closes as it is dropped.)
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let e = client
            .get(&format!("http://127.0.0.1:{port}/"))
            .send()
            .await
            .unwrap_err();
        assert_eq!(e.kind(), ErrorKind::Connect, "{e}");
    });
}

// ---------------------------------------------------------------- redirects

#[test]
fn redirects_are_followed_the_way_browsers_do() {
    giong::block_on(async {
        let other = serve(|req: Request| async move {
            Response::text(format!(
                "other {} auth={}",
                req.target(),
                req.header_str("authorization").unwrap_or("-")
            ))
        });
        let base = serve(move |req: Request| {
            let other = other.clone();
            async move {
                let body = String::from_utf8_lossy(req.body()).into_owned();
                let n: usize = req.query().and_then(|q| q.parse().ok()).unwrap_or(0);
                let to = |status: u16, location: &str| {
                    Response::new(status).header("Location", location)
                };
                match req.path() {
                    "/301" => to(301, "/end?from=301"),
                    "/302" => to(302, "end?from=302"),
                    "/303" => to(303, "/end"),
                    "/307" => to(307, "/end"),
                    "/308" => to(308, "./end"),
                    "/away" => to(302, &format!("{other}/landed")),
                    "/loop" => to(302, &format!("/loop?{}", n + 1)),
                    "/bad" => to(302, "http://exa mple.com/"),
                    "/no-location" => Response::new(302).body("stay"),
                    _ => Response::text(format!(
                        "{} {} auth={} type={} body={body}",
                        req.method(),
                        req.target(),
                        req.header_str("authorization").unwrap_or("-"),
                        req.header_str("content-type").unwrap_or("-"),
                    )),
                }
            }
        });
        let client = Client::new();
        let get = |path: &str| client.get(&format!("{base}{path}"));
        let post = |path: &str| client.post(&format!("{base}{path}")).json("{}");

        let r = get("/301").send().await.unwrap();
        assert_eq!(r.text().unwrap(), "GET /end?from=301 auth=- type=- body=");
        assert_eq!(r.url(), format!("{base}/end?from=301"));
        assert_eq!(
            post("/302").send().await.unwrap().text().unwrap(),
            "GET /end?from=302 auth=- type=- body="
        );
        assert_eq!(
            post("/303").send().await.unwrap().text().unwrap(),
            "GET /end auth=- type=- body="
        );
        assert_eq!(
            post("/307").send().await.unwrap().text().unwrap(),
            "POST /end auth=- type=application/json body={}"
        );
        assert_eq!(
            post("/308").send().await.unwrap().text().unwrap(),
            "POST /end auth=- type=application/json body={}"
        );

        // Credentials stay with their origin.
        let r = get("/away")
            .header("Authorization", "Bearer s3cret")
            .send()
            .await
            .unwrap();
        assert_eq!(r.text().unwrap(), "other /landed auth=-");
        let r = get("/303")
            .header("Authorization", "Bearer s3cret")
            .send()
            .await
            .unwrap();
        assert_eq!(
            r.text().unwrap(),
            "GET /end auth=Bearer s3cret type=- body="
        );

        let e = get("/loop").send().await.unwrap_err();
        assert_eq!(e.kind(), ErrorKind::Redirect);
        assert!(e.to_string().contains("more than 10 times"), "{e}");
        let e = get("/bad").send().await.unwrap_err();
        assert_eq!(e.kind(), ErrorKind::Redirect);
        assert!(e.to_string().contains("not a host name"), "{e}");
        let r = get("/no-location").send().await.unwrap();
        assert_eq!((r.status(), r.text().unwrap()), (302, "stay"));

        let client = Client::builder().redirects(0).build();
        let r = client.get(&format!("{base}/301")).send().await.unwrap();
        assert_eq!(
            (r.status(), r.header("location")),
            (301, Some("/end?from=301"))
        );
        let client = Client::builder().redirects(3).build();
        let e = client
            .get(&format!("{base}/loop"))
            .send()
            .await
            .unwrap_err();
        assert!(
            e.to_string().contains(
                r#"more than 3 times (ClientBuilder::redirects); the last one pointed to "/loop?4""#
            ),
            "{e}"
        );
    });
}

// -------------------------------------------------------------------- HTTPS

/// Starts giong-http's server with TLS; returns its port and its
/// certificate.
fn serve_tls<H, Fut>(handler: H) -> (u16, String)
where
    H: Fn(Request) -> Fut + 'static,
    Fut: Future<Output = Response> + 'static,
{
    let (cert, key) = certificate();
    let acceptor = TlsAcceptor::from_pem(cert.as_bytes(), key.as_bytes()).unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    giong::spawn(async move { Server::new().serve_tls(listener, acceptor, handler).await });
    (port, cert)
}

#[test]
fn https_is_verified_and_kept_alive() {
    giong::block_on(async {
        let (port, cert) = serve_tls(|req: Request| async move {
            match req.path() {
                "/down" => Response::new(302).header("Location", "http://localhost/"),
                _ => Response::text("secret"),
            }
        });
        let url = format!("https://localhost:{port}/");
        let tls = TlsConnector::with_root_pem(cert.as_bytes()).unwrap();
        let client = Client::builder().tls(tls.clone()).build();
        for _ in 0..3 {
            assert_eq!(
                client.get(&url).send().await.unwrap().text().unwrap(),
                "secret"
            );
        }
        assert_eq!(client.connections(&url), (1, 1));

        let e = client
            .get(&format!("https://localhost:{port}/down"))
            .send()
            .await
            .unwrap_err();
        assert_eq!(e.kind(), ErrorKind::Redirect);
        assert!(e.to_string().contains("not encrypted"), "{e}");

        // The certificate is for localhost, not for 127.0.0.1.
        let e = client
            .get(&format!("https://127.0.0.1:{port}/"))
            .send()
            .await
            .unwrap_err();
        assert_eq!(e.kind(), ErrorKind::Connect, "{e}");
        assert!(e.to_string().contains("TLS handshake"), "{e}");

        // Another authority does not vouch for it.
        let (other, _) = certificate();
        let client = Client::builder()
            .tls(TlsConnector::with_root_pem(other.as_bytes()).unwrap())
            .build();
        let e = client.get(&url).send().await.unwrap_err();
        assert_eq!(e.kind(), ErrorKind::Connect, "{e}");

        // HTTP to an HTTPS port: the server's TLS answer is not HTTP.
        let e = Client::new()
            .get(&format!("http://localhost:{port}/"))
            .timeout(Duration::from_secs(10))
            .send()
            .await
            .unwrap_err();
        assert!(
            matches!(e.kind(), ErrorKind::Protocol | ErrorKind::Closed),
            "{e}"
        );
    });
}

#[test]
fn a_tls_handshake_that_never_ends_hits_the_connect_timeout() {
    giong::block_on(async {
        let base = scripted(|_, stream| async move {
            sleep(Duration::from_secs(10)).await;
            drop(stream);
        });
        let port = base.rsplit(':').next().unwrap();
        let client = Client::builder()
            .connect_timeout(Duration::from_millis(200))
            .tls(TlsConnector::with_root_pem(certificate().0.as_bytes()).unwrap())
            .build();
        let e = client
            .get(&format!("https://localhost:{port}/"))
            .send()
            .await
            .unwrap_err();
        assert_eq!(e.kind(), ErrorKind::Connect);
        assert!(e.to_string().contains("connect timeout of 200ms"), "{e}");
        assert!(e.is_transient());
    });
}

#[test]
fn an_https_body_that_ends_with_the_connection_needs_close_notify() {
    giong::block_on(async {
        let (cert, key) = certificate();
        let acceptor = TlsAcceptor::from_pem(cert.as_bytes(), key.as_bytes()).unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        // A response without a length, so its body ends where the connection
        // does; `/clean` ends it with close_notify, `/cut` just closes the
        // socket, as a FIN forged by someone on the path would.
        giong::spawn(async move {
            while let Ok(tcp) = listener.accept().await {
                let acceptor = acceptor.clone();
                giong::spawn(async move {
                    let Ok(tls) = acceptor.accept(tcp).await else {
                        return;
                    };
                    let mut request = Vec::new();
                    while !request.windows(4).any(|w| w == b"\r\n\r\n") {
                        match tls.recv().await {
                            Ok(data) if !data.is_empty() => request.extend_from_slice(&data),
                            _ => return,
                        }
                    }
                    let response = b"HTTP/1.1 200 OK\r\nConnection: close\r\n\r\nthe whole body";
                    let _ = tls.write_all(response.to_vec()).await;
                    if request.starts_with(b"GET /clean ") {
                        let _ = tls.shutdown().await;
                    }
                });
            }
        });
        let client = Client::builder()
            .tls(TlsConnector::with_root_pem(cert.as_bytes()).unwrap())
            .build();
        let clean = format!("https://localhost:{port}/clean");
        let response = client.get(&clean).send().await.unwrap();
        assert_eq!(response.text().unwrap(), "the whole body");

        let cut = format!("https://localhost:{port}/cut");
        let e = client.get(&cut).send().await.unwrap_err();
        assert_eq!(e.kind(), ErrorKind::Closed, "{e}");
        assert!(e.to_string().contains("close_notify"), "{e}");
    });
}

/// The real thing: a public HTTPS API, through the machine's certificate
/// authorities. Needs the internet, so it runs only when asked:
/// `cargo test -p giong-http --all-features --test client -- --ignored`.
#[test]
#[ignore = "needs the internet"]
fn fetches_from_a_public_https_api() {
    giong::block_on(async {
        let r = Client::new()
            .get("https://crates.io/api/v1/crates/giong")
            .header("accept", "application/json")
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap();
        assert!(r.text().unwrap().contains(r#""name":"giong""#));
    });
}
