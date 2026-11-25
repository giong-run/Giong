//! A throwaway Redis server for the tests, started once per test binary
//! and killed when the binary exits.
//!
//! `redis-server` is found through `GIONG_REDIS_BIN` (the directory that
//! holds it), then at `/usr/bin/redis-server`, then on `PATH`. Without it
//! the tests are skipped with a message, unless `GIONG_REDIS_REQUIRED=1`,
//! which makes that a failure (CI sets it).

use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use giong_redis::{Config, Tls, TlsConnector};

pub struct Server {
    pub port: u16,
    pub tls_port: u16,
    pub socket: PathBuf,
    pub ca_pem: Vec<u8>,
    /// `redis` or `valkey`, as the server's answer to `HELLO` says.
    pub name: String,
    /// The version the server's answer to `HELLO` gives.
    pub version: String,
    dir: PathBuf,
}

/// An ACL user with every permission.
pub const APP: (&str, &str) = ("app", "app-secret");

static SERVER: OnceLock<Option<Result<Server, String>>> = OnceLock::new();
static PID: AtomicI32 = AtomicI32::new(0);
static DIR: OnceLock<PathBuf> = OnceLock::new();

/// The server, or `None` (after saying why) when Redis is not installed.
///
/// It is started once. If that fails, every test fails with the reason:
/// a panic inside `get_or_init` would leave it unset, and the next test
/// would start a second server (and fail on something unrelated).
pub fn server() -> Option<&'static Server> {
    let started = SERVER.get_or_init(|| {
        let Some(bin) = find_bin() else {
            if std::env::var_os("GIONG_REDIS_REQUIRED").is_some_and(|v| v == "1") {
                panic!(
                    "GIONG_REDIS_REQUIRED=1 but redis-server was not found; set GIONG_REDIS_BIN"
                );
            }
            eprintln!("skipping: redis-server not found (set GIONG_REDIS_BIN)");
            return None;
        };
        Some(std::panic::catch_unwind(|| start(bin)).map_err(|panic| {
            panic
                .downcast_ref::<String>()
                .cloned()
                .or_else(|| panic.downcast_ref::<&str>().map(|s| (*s).to_owned()))
                .unwrap_or_default()
        }))
    });
    match started {
        None => None,
        Some(Ok(server)) => Some(server),
        Some(Err(why)) => panic!("the test server failed to start: {why}"),
    }
}

impl Server {
    /// The default user over TCP to 127.0.0.1.
    pub fn tcp(&self) -> Config {
        Config::new().host("127.0.0.1").port(self.port)
    }

    /// The default user over the Unix socket.
    pub fn unix(&self) -> Config {
        Config::new().host(self.socket.to_str().unwrap())
    }

    /// The default user over TLS, trusting the test authority.
    pub fn tls(&self, host: &str) -> Config {
        Config::new()
            .host(host)
            .port(self.tls_port)
            .tls(Tls::Verify(
                TlsConnector::with_root_pem(&self.ca_pem).unwrap(),
            ))
    }

    pub fn ca_path(&self) -> PathBuf {
        self.dir.join("ca.pem")
    }
}

fn find_bin() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("GIONG_REDIS_BIN") {
        let bin = PathBuf::from(dir).join(exe("redis-server"));
        return bin.exists().then_some(bin);
    }
    let path_dirs: Vec<PathBuf> = std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).collect())
        .unwrap_or_default();
    std::iter::once(PathBuf::from("/usr/bin"))
        .chain(path_dirs)
        .map(|d| d.join(exe("redis-server")))
        .find(|p| p.exists())
}

/// Kills the process `pid` this binary started and waits for it to go.
#[cfg(unix)]
fn kill(pid: i32) {
    // SAFETY: plain system calls on a child process we started.
    unsafe {
        libc::kill(pid, libc::SIGKILL);
        libc::waitpid(pid, std::ptr::null_mut(), 0);
    }
}

#[cfg(windows)]
fn kill(pid: i32) {
    let _ = std::process::Command::new("taskkill")
        .args(["/F", "/T", "/PID", &pid.to_string()])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
}

/// `name` with the system's suffix for programs (`.exe` on Windows).
fn exe(name: &str) -> String {
    format!("{name}{}", std::env::consts::EXE_SUFFIX)
}

extern "C" fn stop_server() {
    let pid = PID.load(Ordering::SeqCst);
    if pid > 0 {
        kill(pid);
    }
    if let Some(dir) = DIR.get() {
        let _ = std::fs::remove_dir_all(dir);
    }
}

/// A port on 127.0.0.1 that refuses connections for as long as the
/// returned socket lives: it is bound there and not listening, so nothing
/// else can take the port and answer in the meantime.
pub fn closed_port() -> (socket2::Socket, u16) {
    use socket2::{Domain, Socket, Type};
    let sock = Socket::new(Domain::IPV4, Type::STREAM, None).expect("socket");
    sock.bind(&std::net::SocketAddr::from(([127, 0, 0, 1], 0)).into())
        .expect("bind");
    let port = sock
        .local_addr()
        .expect("getsockname")
        .as_socket()
        .expect("an IPv4 address")
        .port();
    (sock, port)
}

/// A port for the server that nothing listens on now, below the kernel's
/// ephemeral range.
///
/// Redis cannot be told to pick a port itself and say which (`--port 0`
/// turns TCP off), so the test picks one and the server binds it a moment
/// later. A port from the ephemeral range (what binding port 0 gives) can be
/// taken in that moment by any socket of the tests or of another job on
/// the machine: Valkey 8.0 then failed to bind its TLS port and aborted, in
/// a merge-queue run. Ports below the range are given out only on request,
/// and the server is started again on other ports if one is taken anyway.
fn server_port(taken: &[u16]) -> u16 {
    let low = std::fs::read_to_string("/proc/sys/net/ipv4/ip_local_port_range")
        .ok()
        .and_then(|r| r.split_whitespace().next()?.parse::<u16>().ok())
        .unwrap_or(32768);
    let (from, to) = (10_000u16, low.max(11_000));
    let mut seed = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .subsec_nanos()
        ^ std::process::id().wrapping_mul(2_654_435_761);
    loop {
        // xorshift: any spread will do.
        seed ^= seed << 13;
        seed ^= seed >> 17;
        seed ^= seed << 5;
        let port = from + (seed % u32::from(to - from)) as u16;
        if !taken.contains(&port) && std::net::TcpListener::bind(("127.0.0.1", port)).is_ok() {
            return port;
        }
    }
}

/// Times the server is started on new ports when one of its ports was
/// taken before it could bind it.
const START_ATTEMPTS: usize = 5;

fn start(bin: PathBuf) -> Server {
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos()
        % 1_000_000_000;
    // Short: the socket's path must fit in 107 bytes.
    let dir = std::env::temp_dir().join(format!("giong-rd-{}-{stamp}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    DIR.set(dir.clone())
        .expect("the test server is started once per binary");
    // SAFETY: registering a plain function to run at exit.
    unsafe { libc::atexit(stop_server) };

    // A certificate authority and a server certificate for TLS.
    let ca_key = rcgen::KeyPair::generate().unwrap();
    let mut ca_params = rcgen::CertificateParams::new(Vec::<String>::new()).unwrap();
    ca_params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
    ca_params
        .distinguished_name
        .push(rcgen::DnType::CommonName, "giong-redis test CA");
    let ca = ca_params.self_signed(&ca_key).unwrap();
    let server_key = rcgen::KeyPair::generate().unwrap();
    let server_params =
        rcgen::CertificateParams::new(vec!["localhost".to_owned(), "127.0.0.1".to_owned()])
            .unwrap();
    let server_cert = server_params.signed_by(&server_key, &ca, &ca_key).unwrap();
    std::fs::write(dir.join("ca.pem"), ca.pem()).unwrap();
    std::fs::write(dir.join("server.pem"), server_cert.pem()).unwrap();
    std::fs::write(dir.join("server.key"), server_key.serialize_pem()).unwrap();

    let socket = dir.join("redis.sock");
    let debug = major_version(&bin) >= 7;
    let mut taken = Vec::new();
    let (port, tls_port, name, version) = loop {
        let port = server_port(&taken);
        let tls_port = server_port(&[&taken[..], &[port]].concat());
        taken.extend([port, tls_port]);
        match launch(&bin, &dir, &socket, port, tls_port, debug) {
            Ok((name, version)) => break (port, tls_port, name, version),
            Err(log)
                if log.contains("Address already in use") && taken.len() < 2 * START_ATTEMPTS =>
            {
                eprintln!("redis-server: port {port} or {tls_port} was taken; trying others");
            }
            Err(log) => panic!("redis-server did not start: {log}"),
        }
    };
    let deadline = Instant::now() + Duration::from_secs(20);
    while !socket.exists() {
        assert!(
            Instant::now() < deadline,
            "redis-server made no Unix socket"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    Server {
        port,
        tls_port,
        socket,
        ca_pem: ca.pem().into_bytes(),
        name,
        version,
        dir,
    }
}

/// Starts `redis-server` on `port` and `tls_port` and waits until it
/// answers, giving its name and version; or, if it exits first, its log.
fn launch(
    bin: &std::path::Path,
    dir: &std::path::Path,
    socket: &std::path::Path,
    port: u16,
    tls_port: u16,
    debug: bool,
) -> Result<(String, String), String> {
    // A failed attempt may leave its socket behind.
    let _ = std::fs::remove_file(socket);
    let log_path = dir.join("server.log");
    let log = std::fs::File::create(&log_path).unwrap();
    let mut cmd = Command::new(bin);
    cmd.args(["--port", &port.to_string()])
        .args(["--bind", "127.0.0.1"])
        .args(["--tls-port", &tls_port.to_string()])
        .arg("--tls-cert-file")
        .arg(dir.join("server.pem"))
        .arg("--tls-key-file")
        .arg(dir.join("server.key"))
        .arg("--tls-ca-cert-file")
        .arg(dir.join("ca.pem"))
        .args(["--tls-auth-clients", "no"])
        .arg("--unixsocket")
        .arg(socket)
        .args(["--unixsocketperm", "700"])
        .arg("--dir")
        .arg(dir)
        .args(["--save", ""])
        .args(["--appendonly", "no"])
        .args(["--databases", "16"])
        // Each word of a setting is an argument of its own.
        .args(["--user", "default", "on", "nopass", "~*", "&*", "+@all"])
        .args([
            "--user",
            APP.0,
            "on",
            &format!(">{}", APP.1),
            "~*",
            "&*",
            "+@all",
        ])
        .stdout(Stdio::from(log.try_clone().unwrap()))
        .stderr(Stdio::from(log));
    // DEBUG (for DEBUG SLEEP) is off by default since Redis 7.0, which added
    // this setting; Redis 6.2 does not know it, and allows DEBUG anyway.
    if debug {
        cmd.args(["--enable-debug-command", "yes"]);
    }
    // The server dies with the thread that started it, even when the test
    // binary is killed; the thread waits for it, so lives as long as it, and
    // says when it exits.
    let (started_tx, started) = std::sync::mpsc::channel();
    let (exited_tx, exited) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        #[cfg(target_os = "linux")]
        use std::os::unix::process::CommandExt;
        // Linux only: elsewhere a killed test binary leaves the server
        // running until the test's own cleanup or the CI job ends.
        #[cfg(target_os = "linux")]
        // SAFETY: prctl is async-signal-safe and touches only this process.
        unsafe {
            cmd.pre_exec(|| {
                libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL);
                Ok(())
            });
        }
        let mut child = cmd.spawn().expect("start redis-server");
        started_tx.send(child.id()).unwrap();
        let _ = child.wait();
        let _ = exited_tx.send(());
    });
    PID.store(started.recv().unwrap() as i32, Ordering::SeqCst);

    let deadline = Instant::now() + Duration::from_secs(20);
    let log = || std::fs::read_to_string(&log_path).unwrap_or_default();
    loop {
        // Valkey keeps `redis_version` at the Redis release it is compatible
        // with (7.2.4 for Valkey 8.0) and adds `server_name` and
        // `valkey_version`, which are what its `HELLO` reports. Redis has
        // neither.
        if let Ok(mut s) = std::net::TcpStream::connect(("127.0.0.1", port)) {
            use std::io::{Read, Write};
            s.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
            if s.write_all(b"INFO server\r\n").is_ok() {
                let mut buf = vec![0u8; 8192];
                if let Ok(n) = s.read(&mut buf) {
                    let text = String::from_utf8_lossy(&buf[..n]).into_owned();
                    let field = |prefix: &str| {
                        text.lines()
                            .find_map(|l| l.strip_prefix(prefix))
                            .map(|v| v.trim().to_owned())
                    };
                    if let Some(redis_version) = field("redis_version:") {
                        return Ok((
                            field("server_name:").unwrap_or_else(|| "redis".to_owned()),
                            field("valkey_version:").unwrap_or(redis_version),
                        ));
                    }
                }
            }
        }
        if exited.try_recv().is_ok() {
            PID.store(0, Ordering::SeqCst);
            return Err(log());
        }
        if Instant::now() >= deadline {
            return Err(format!("no answer within 20 s: {}", log()));
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// The major version `redis-server --version` reports (`v=7.0.15`; Valkey
/// says `v=8.0.1` too), or 0 if it cannot be read.
fn major_version(bin: &std::path::Path) -> u32 {
    let Ok(out) = Command::new(bin).arg("--version").output() else {
        return 0;
    };
    let text = String::from_utf8_lossy(&out.stdout);
    text.split_whitespace()
        .find_map(|w| w.strip_prefix("v="))
        .and_then(|v| v.split('.').next())
        .and_then(|m| m.parse().ok())
        .unwrap_or(0)
}
