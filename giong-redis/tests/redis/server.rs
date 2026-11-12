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
    pub version: String,
    dir: PathBuf,
}

/// An ACL user with every permission.
pub const APP: (&str, &str) = ("app", "app-secret");

static SERVER: OnceLock<Option<Server>> = OnceLock::new();
static PID: AtomicI32 = AtomicI32::new(0);
static DIR: OnceLock<PathBuf> = OnceLock::new();

/// The server, or `None` (after saying why) when Redis is not installed.
pub fn server() -> Option<&'static Server> {
    SERVER
        .get_or_init(|| {
            let Some(bin) = find_bin() else {
                if std::env::var_os("GIONG_REDIS_REQUIRED").is_some_and(|v| v == "1") {
                    panic!("GIONG_REDIS_REQUIRED=1 but redis-server was not found; set GIONG_REDIS_BIN");
                }
                eprintln!("skipping: redis-server not found (set GIONG_REDIS_BIN)");
                return None;
            };
            Some(start(bin))
        })
        .as_ref()
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
        let bin = PathBuf::from(dir).join("redis-server");
        return bin.exists().then_some(bin);
    }
    let path_dirs: Vec<PathBuf> = std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).collect())
        .unwrap_or_default();
    std::iter::once(PathBuf::from("/usr/bin"))
        .chain(path_dirs)
        .map(|d| d.join("redis-server"))
        .find(|p| p.exists())
}

extern "C" fn stop_server() {
    let pid = PID.load(Ordering::SeqCst);
    if pid > 0 {
        // SAFETY: plain system calls on a child process we started.
        unsafe {
            libc::kill(pid, libc::SIGKILL);
            libc::waitpid(pid, std::ptr::null_mut(), 0);
        }
    }
    if let Some(dir) = DIR.get() {
        let _ = std::fs::remove_dir_all(dir);
    }
}

pub fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn start(bin: PathBuf) -> Server {
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos()
        % 1_000_000_000;
    // Short: the socket's path must fit in 107 bytes.
    let dir = std::env::temp_dir().join(format!("giong-rd-{}-{stamp}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    DIR.set(dir.clone()).unwrap();
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

    let port = free_port();
    let tls_port = free_port();
    let socket = dir.join("redis.sock");
    let log = std::fs::File::create(dir.join("server.log")).unwrap();
    let mut cmd = Command::new(&bin);
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
        .arg(&socket)
        .args(["--unixsocketperm", "700"])
        .arg("--dir")
        .arg(&dir)
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
    if major_version(&bin) >= 7 {
        cmd.args(["--enable-debug-command", "yes"]);
    }
    // The server dies with the thread that started it, even when the test
    // binary is killed; the thread waits for it, so lives as long as it.
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        use std::os::unix::process::CommandExt;
        // SAFETY: prctl is async-signal-safe and touches only this process.
        unsafe {
            cmd.pre_exec(|| {
                libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL);
                Ok(())
            });
        }
        let mut child = cmd.spawn().expect("start redis-server");
        tx.send(child.id()).unwrap();
        let _ = child.wait();
    });
    PID.store(rx.recv().unwrap() as i32, Ordering::SeqCst);

    let deadline = Instant::now() + Duration::from_secs(20);
    let version = loop {
        if let Ok(mut s) = std::net::TcpStream::connect(("127.0.0.1", port)) {
            use std::io::{Read, Write};
            s.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
            if s.write_all(b"INFO server\r\n").is_ok() {
                let mut buf = vec![0u8; 8192];
                if let Ok(n) = s.read(&mut buf) {
                    let text = String::from_utf8_lossy(&buf[..n]).into_owned();
                    if let Some(v) = text.lines().find_map(|l| l.strip_prefix("redis_version:")) {
                        break v.trim().to_owned();
                    }
                }
            }
        }
        assert!(
            Instant::now() < deadline,
            "redis-server did not start: {}",
            std::fs::read_to_string(dir.join("server.log")).unwrap_or_default()
        );
        std::thread::sleep(Duration::from_millis(50));
    };
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
        version,
        dir,
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
