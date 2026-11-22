//! `giong dashboard`: a page on a loopback port that reads the service's
//! log and can send it requests. What matters most is who can reach it: only
//! this machine, and no other web page open in the same browser (DNS
//! rebinding, cross-site requests). It serves until stopped, so the blind
//! contract run skips it and this file holds its contract.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::Value;

use crate::harness::*;

struct Dashboard {
    child: Child,
    first_line: String,
    port: u16,
}

impl Drop for Dashboard {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn start(dir: &std::path::Path, port: u16) -> Dashboard {
    let mut child = Command::new(GIONG)
        .args(["dashboard", "--port", &port.to_string(), "--json"])
        .current_dir(dir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("start giong dashboard");
    let mut first_line = String::new();
    BufReader::new(child.stdout.take().unwrap())
        .read_line(&mut first_line)
        .unwrap();
    let start = Instant::now();
    while TcpStream::connect(("127.0.0.1", port)).is_err() {
        assert!(
            start.elapsed() < Duration::from_secs(20),
            "the dashboard never listened"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    Dashboard {
        child,
        first_line,
        port,
    }
}

/// A raw request: (status, headers and body as text).
fn raw(port: u16, request: &str) -> (u16, String) {
    let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    s.write_all(request.as_bytes()).unwrap();
    let mut out = Vec::new();
    let _ = s.read_to_end(&mut out);
    let text = String::from_utf8_lossy(&out).into_owned();
    let status = text
        .split_whitespace()
        .nth(1)
        .and_then(|c| c.parse().ok())
        .unwrap_or(0);
    (status, text)
}

fn get(port: u16, path: &str, headers: &str) -> (u16, String) {
    raw(
        port,
        &format!("GET {path} HTTP/1.1\r\n{headers}Connection: close\r\n\r\n"),
    )
}

#[test]
fn it_answers_its_own_page_and_refuses_other_hosts_and_sites() {
    if !has_command("dashboard") {
        return;
    }
    let dir = TempDir::new("dashboard-who");
    let d = start(dir.path(), free_port());
    let p = d.port;
    let host = format!("Host: 127.0.0.1:{p}\r\n");

    let (s, page) = get(p, "/", &host);
    assert_eq!(s, 200, "{page}");
    assert!(
        page.to_ascii_lowercase().contains("x-frame-options: deny"),
        "{page}"
    );

    // DNS rebinding: a name that resolves to 127.0.0.1 but is not ours.
    for h in ["Host: evil.example\r\n", "Host: evil.example:{p}\r\n", ""] {
        let h = h.replace("{p}", &p.to_string());
        let (s, body) = get(p, "/", &h);
        assert_eq!(s, 403, "Host {h:?} was let in: {body}");
    }
    // Another page in the browser.
    let (s, _) = get(p, "/", &format!("{host}Origin: http://evil.example\r\n"));
    assert_eq!(s, 403);
    let (s, _) = get(p, "/", &format!("{host}Sec-Fetch-Site: cross-site\r\n"));
    assert_eq!(s, 403);
    // The API wants its header, which a plain form or <img> cannot send.
    let (s, _) = get(p, "/api/state", &host);
    assert_eq!(s, 403);
    let (s, body) = raw(
        p,
        &format!(
            "POST /api/send HTTP/1.1\r\n{host}Origin: http://evil.example\r\n\
             X-Giong-Dashboard: 1\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{{}}"
        ),
    );
    assert_eq!(s, 403, "a cross-origin POST to the API was let in: {body}");
    // Two Host headers, in either order, are refused (RFC 9112: 400).
    for two in [
        format!("Host: evil.example\r\n{host}"),
        format!("{host}Host: evil.example\r\n"),
        format!("{host}Host:\r\n"),
    ] {
        let (s, body) = get(p, "/", &two);
        assert!(matches!(s, 400 | 403), "{two:?} was let in: {body}");
    }
}

#[test]
fn it_listens_on_loopback_only() {
    if !has_command("dashboard") {
        return;
    }
    // This machine's address on its outgoing interface (no packet is sent).
    let Some(ip) = std::net::UdpSocket::bind("0.0.0.0:0")
        .and_then(|u| u.connect("10.255.255.255:9").map(|()| u))
        .and_then(|u| u.local_addr())
        .ok()
        .map(|a| a.ip())
        .filter(|ip| !ip.is_loopback() && !ip.is_unspecified())
    else {
        eprintln!("SKIP: no non-loopback address here");
        return;
    };
    let dir = TempDir::new("dashboard-loopback");
    let d = start(dir.path(), free_port());
    let reached = TcpStream::connect_timeout(&SocketAddr::new(ip, d.port), Duration::from_secs(2));
    assert!(reached.is_err(), "the dashboard answers on {ip}:{}", d.port);
}

#[test]
fn a_taken_port_is_a_coded_failure_at_once() {
    if !has_command("dashboard") {
        return;
    }
    let taken = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = taken.local_addr().unwrap().port();
    let dir = TempDir::new("dashboard-taken");
    let out = giong(["dashboard", "--port", &port.to_string(), "--json"])
        .dir(dir.path())
        .timeout(Duration::from_secs(20))
        .run();
    let v = out.report("dashboard");
    out.expect_code(2);
    assert!(v["error"]["code"].is_string(), "{}", out.dump());
}

/// The line it prints before it serves is the one JSON document of the
/// command, in the report's shape, so an agent reads the URL the way it reads
/// every other command.
#[test]
fn the_json_line_is_a_report() {
    if !has_command("dashboard") {
        return;
    }
    let dir = TempDir::new("dashboard-json");
    let d = start(dir.path(), free_port());
    let v: Value = crate::schema::parse_single_document(&d.first_line)
        .unwrap_or_else(|e| panic!("{e}: {:?}", d.first_line));
    crate::schema::check_report(&v, "dashboard").unwrap_or_else(|e| panic!("{e}: {v}"));
    assert!(v["data"]["url"]
        .as_str()
        .unwrap()
        .starts_with("http://127.0.0.1:"));
}
