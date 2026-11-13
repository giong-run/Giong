//! Opens many connections to an HTTP server and holds them, so that
//! bench/memory.sh can read how much memory the server spends per connection.
//!
//! `hold ADDR CONNS MODE [PATH]`, MODE one of:
//!
//! * `open`: connect and send nothing.
//! * `idle`: send one keep-alive request on each connection, read the
//!   response, then leave the connection idle (the usual keep-alive client).
//! * `pending`: send one request for PATH on each connection and do not wait
//!   for the answer (PATH should be slow, like `/wait?ms=600000`): every
//!   connection then has a request in the middle of being handled, as when a
//!   handler waits for a database or another API.
//!
//! Prints `ready` once every connection is in place, then holds them until
//! killed. Connections come from 127.0.0.1 to 127.0.0.250 in turn, so that
//! the run does not depend on the local port range.
use std::io::{self, Read, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpStream};
use std::os::fd::FromRawFd;
use std::time::Instant;

fn connect_from(src: Ipv4Addr, dst: SocketAddr) -> io::Result<TcpStream> {
    let SocketAddr::V4(dst) = dst else {
        return TcpStream::connect(dst);
    };
    // SAFETY: plain socket calls on a descriptor owned by the TcpStream below.
    unsafe {
        let fd = libc::socket(libc::AF_INET, libc::SOCK_STREAM | libc::SOCK_CLOEXEC, 0);
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        let stream = TcpStream::from_raw_fd(fd);
        let mut sa: libc::sockaddr_in = std::mem::zeroed();
        sa.sin_family = libc::AF_INET as _;
        sa.sin_addr.s_addr = u32::from(src).to_be();
        if libc::bind(fd, &sa as *const _ as *const _, std::mem::size_of_val(&sa) as _) < 0 {
            return Err(io::Error::last_os_error());
        }
        sa.sin_port = dst.port().to_be();
        sa.sin_addr.s_addr = u32::from(*dst.ip()).to_be();
        if libc::connect(fd, &sa as *const _ as *const _, std::mem::size_of_val(&sa) as _) < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(stream)
    }
}

/// Reads one response with a Content-Length body.
fn read_response(stream: &mut TcpStream, buf: &mut Vec<u8>) -> io::Result<()> {
    buf.clear();
    let mut chunk = [0u8; 4096];
    loop {
        let n = stream.read(&mut chunk)?;
        if n == 0 {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "closed"));
        }
        buf.extend_from_slice(&chunk[..n]);
        if let Some(end) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            let head = String::from_utf8_lossy(&buf[..end]).to_ascii_lowercase();
            let len = head
                .lines()
                .find_map(|l| l.strip_prefix("content-length:"))
                .and_then(|v| v.trim().parse::<usize>().ok())
                .unwrap_or(0);
            if buf.len() >= end + 4 + len {
                return Ok(());
            }
        }
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 4 {
        eprintln!("usage: hold ADDR CONNS open|idle|pending [PATH]");
        std::process::exit(2);
    }
    let addr: SocketAddr = args[1].parse().expect("ADDR");
    let conns: usize = args[2].parse().expect("CONNS");
    let mode = args[3].as_str();
    let path = args.get(4).map(String::as_str).unwrap_or("/json");
    let request = format!("GET {path} HTTP/1.1\r\nHost: bench\r\n\r\n");

    let start = Instant::now();
    let mut streams = Vec::with_capacity(conns);
    let mut buf = Vec::new();
    for i in 0..conns {
        let src = Ipv4Addr::new(127, 0, 0, 1 + (i % 250) as u8);
        let mut stream = match connect_from(src, addr) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("connection {i}: {e}");
                std::process::exit(1);
            }
        };
        match mode {
            "open" => {}
            "idle" => {
                stream.write_all(request.as_bytes()).expect("write");
                if let Err(e) = read_response(&mut stream, &mut buf) {
                    eprintln!("connection {i}: {e}");
                    std::process::exit(1);
                }
            }
            "pending" => stream.write_all(request.as_bytes()).expect("write"),
            _ => {
                eprintln!("unknown mode {mode}");
                std::process::exit(2);
            }
        }
        streams.push(stream);
    }
    println!("ready {} connections in {:.2} s", streams.len(), start.elapsed().as_secs_f64());
    io::stdout().flush().unwrap();
    loop {
        std::thread::park();
    }
}
