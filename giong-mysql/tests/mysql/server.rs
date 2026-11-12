//! A throwaway MySQL server for the tests: a fresh data directory in a
//! temporary directory, started once per test binary and killed when the
//! binary exits.
//!
//! `mysqld` is found through `GIONG_MYSQL_BIN` (the directory that holds it,
//! such as the `bin` of a MySQL tarball), then at `/usr/sbin/mysqld` (Debian,
//! Ubuntu and the GitHub runners), then on `PATH`; the `mysql` client next
//! to it or on `PATH`. Without them the tests are skipped with a message,
//! unless `GIONG_MYSQL_REQUIRED=1`, which makes that a failure (CI sets it).

use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use giong_mysql::{Config, Tls, TlsConnector};

pub struct Server {
    pub port: u16,
    pub socket: PathBuf,
    pub ca_pem: Vec<u8>,
    /// The server's RSA key for `caching_sha2_password`.
    pub public_key_pem: Vec<u8>,
    /// Whether the server has the `mysql_native_password` plugin (gone in
    /// MySQL 9).
    pub has_native: bool,
    /// Whether it has `sha256_password`.
    pub has_sha256: bool,
    pub version: String,
    dir: PathBuf,
    bins: Bins,
}

#[derive(Clone)]
struct Bins {
    mysqld: PathBuf,
    mysql: PathBuf,
}

/// Users the server is set up with, and their passwords. Each test that
/// cares which authentication path a login takes has a user of its own: the
/// server's cache of password hashes is per user.
pub const SHA2_USER: (&str, &str) = ("sha2_user", "sha2-secret");
pub const RSA_USER: (&str, &str) = ("rsa_user", "rsa-secret");
pub const KEY_USER: (&str, &str) = ("key_user", "key-secret");
pub const FAST_USER: (&str, &str) = ("fast_user", "fast-secret");
pub const TLS_USER: (&str, &str) = ("tls_user", "tls-secret");
pub const NATIVE_USER: (&str, &str) = ("native_user", "native-secret");
pub const SHA256_USER: (&str, &str) = ("sha256_user", "sha256-secret");

/// The database the tests work in.
pub const DATABASE: &str = "giong";

static SERVER: OnceLock<Option<Server>> = OnceLock::new();
static PID: AtomicI32 = AtomicI32::new(0);
static DIR: OnceLock<PathBuf> = OnceLock::new();

/// The server, or `None` (after saying why) when MySQL is not installed.
pub fn server() -> Option<&'static Server> {
    SERVER
        .get_or_init(|| {
            let Some(bins) = find_bins() else {
                if std::env::var_os("GIONG_MYSQL_REQUIRED").is_some_and(|v| v == "1") {
                    panic!(
                        "GIONG_MYSQL_REQUIRED=1 but mysqld and mysql were not found; set \
                         GIONG_MYSQL_BIN"
                    );
                }
                eprintln!("skipping: mysqld and mysql not found (set GIONG_MYSQL_BIN)");
                return None;
            };
            Some(start(bins))
        })
        .as_ref()
}

impl Server {
    /// `root` over the Unix socket (no password).
    pub fn local(&self) -> Config {
        Config::new()
            .host(self.socket.to_str().unwrap())
            .user("root")
            .database(DATABASE)
    }

    /// A user over TCP to 127.0.0.1, without TLS.
    pub fn tcp(&self, (user, password): (&str, &str)) -> Config {
        Config::new()
            .host("127.0.0.1")
            .port(self.port)
            .user(user)
            .password(password)
            .database(DATABASE)
    }

    /// A user over the Unix socket.
    pub fn socket(&self, (user, password): (&str, &str)) -> Config {
        Config::new()
            .host(self.socket.to_str().unwrap())
            .user(user)
            .password(password)
            .database(DATABASE)
    }

    /// A user over TCP with TLS, trusting the test authority.
    pub fn tls(&self, host: &str, (user, password): (&str, &str)) -> Config {
        Config::new()
            .host(host)
            .port(self.port)
            .user(user)
            .password(password)
            .database(DATABASE)
            .tls(Tls::Verify(
                TlsConnector::with_root_pem(&self.ca_pem).unwrap(),
            ))
    }

    /// Runs SQL as root with the `mysql` client and returns what it prints,
    /// tab-separated.
    pub fn sql(&self, sql: &str) -> String {
        let out = Command::new(&self.bins.mysql)
            .args(["--no-defaults", "-uroot", "-N", "-B", "-S"])
            .arg(&self.socket)
            .args(["-e", sql])
            .output()
            .expect("run mysql");
        assert!(
            out.status.success(),
            "mysql failed on {sql:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout).unwrap()
    }

    /// Like [`sql`](Self::sql), but returns whether it worked.
    fn try_sql(&self, sql: &str) -> bool {
        Command::new(&self.bins.mysql)
            .args(["--no-defaults", "-uroot", "-N", "-B", "-S"])
            .arg(&self.socket)
            .args(["-e", sql])
            .stderr(Stdio::null())
            .stdout(Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
    }

    pub fn log(&self) -> String {
        std::fs::read_to_string(self.dir.join("server.log")).unwrap_or_default()
    }
}

fn find_bins() -> Option<Bins> {
    let path_dirs: Vec<PathBuf> = std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).collect())
        .unwrap_or_default();
    let mysqld = match std::env::var_os("GIONG_MYSQL_BIN") {
        Some(dir) => PathBuf::from(dir).join("mysqld"),
        None => std::iter::once(PathBuf::from("/usr/sbin"))
            .chain(path_dirs.iter().cloned())
            .map(|d| d.join("mysqld"))
            .find(|p| p.exists())?,
    };
    if !mysqld.exists() {
        return None;
    }
    let mysql = std::iter::once(mysqld.parent()?.to_owned())
        .chain(path_dirs)
        .chain([PathBuf::from("/usr/bin")])
        .map(|d| d.join("mysql"))
        .find(|p| p.exists())?;
    Some(Bins { mysqld, mysql })
}

fn is_root() -> bool {
    // SAFETY: geteuid has no preconditions.
    unsafe { libc::geteuid() == 0 }
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

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn mysqld(bins: &Bins) -> Command {
    let mut cmd = Command::new(&bins.mysqld);
    cmd.arg("--no-defaults");
    // mysqld refuses to run as root unless told to.
    if is_root() {
        cmd.arg("--user=root");
    }
    cmd
}

fn start(bins: Bins) -> Server {
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos()
        % 1_000_000_000;
    // Short: the socket's path must fit in 107 bytes.
    let dir = std::env::temp_dir().join(format!("giong-my-{}-{stamp}", std::process::id()));
    let data = dir.join("data");
    std::fs::create_dir_all(&dir).unwrap();
    DIR.set(dir.clone()).unwrap();
    // SAFETY: registering a plain function to run at exit.
    unsafe { libc::atexit(stop_server) };

    let out = mysqld(&bins)
        .arg("--initialize-insecure")
        .arg(format!("--datadir={}", data.display()))
        .arg(format!("--log-error={}", dir.join("init.log").display()))
        .output()
        .expect("run mysqld --initialize-insecure");
    assert!(
        out.status.success(),
        "mysqld --initialize-insecure failed: {}{}",
        String::from_utf8_lossy(&out.stderr),
        std::fs::read_to_string(dir.join("init.log")).unwrap_or_default()
    );

    // A certificate authority and a server certificate for TLS.
    let ca_key = rcgen::KeyPair::generate().unwrap();
    let mut ca_params = rcgen::CertificateParams::new(Vec::<String>::new()).unwrap();
    ca_params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
    ca_params
        .distinguished_name
        .push(rcgen::DnType::CommonName, "giong-mysql test CA");
    let ca = ca_params.self_signed(&ca_key).unwrap();
    let server_key = rcgen::KeyPair::generate().unwrap();
    let server_params =
        rcgen::CertificateParams::new(vec!["localhost".to_owned(), "127.0.0.1".to_owned()])
            .unwrap();
    let server_cert = server_params.signed_by(&server_key, &ca, &ca_key).unwrap();
    std::fs::write(dir.join("ca.pem"), ca.pem()).unwrap();
    std::fs::write(dir.join("server-cert.pem"), server_cert.pem()).unwrap();
    std::fs::write(dir.join("server-key.pem"), server_key.serialize_pem()).unwrap();

    let port = free_port();
    let socket = dir.join("mysqld.sock");

    // The server is started from a thread that never ends, with SIGKILL as
    // its parent-death signal: if this process dies without running its exit
    // handlers, the server goes too.
    let (tx, rx) = std::sync::mpsc::channel();
    let mut cmd = mysqld(&bins);
    cmd.arg(format!("--datadir={}", data.display()))
        .arg(format!("--port={port}"))
        .arg("--bind-address=127.0.0.1,127.0.0.2")
        .arg(format!("--socket={}", socket.display()))
        .arg(format!("--pid-file={}", dir.join("mysqld.pid").display()))
        .arg(format!("--log-error={}", dir.join("server.log").display()))
        .arg(format!("--ssl-ca={}", dir.join("ca.pem").display()))
        .arg(format!(
            "--ssl-cert={}",
            dir.join("server-cert.pem").display()
        ))
        .arg(format!(
            "--ssl-key={}",
            dir.join("server-key.pem").display()
        ))
        .args([
            "--mysqlx=OFF",
            "--secure-file-priv=NULL",
            "--skip-name-resolve",
            "--skip-log-bin",
            "--innodb-flush-log-at-trx-commit=0",
            "--innodb-doublewrite=0",
            "--max-connections=300",
            "--max-allowed-packet=64M",
            // MySQL 8.4 has the plugin off by default; 8.0 does not know the
            // option, which `loose` lets it ignore.
            "--loose-mysql-native-password=ON",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    std::thread::spawn(move || {
        // SAFETY: prctl is async-signal-safe and touches only this process.
        unsafe {
            cmd.pre_exec(|| {
                libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL);
                Ok(())
            });
        }
        let mut child = cmd.spawn().expect("start mysqld");
        tx.send(child.id()).unwrap();
        // Blocks until the server is gone, keeping this thread (its parent)
        // alive until then.
        let _ = child.wait();
    });
    let pid = rx.recv().unwrap();
    PID.store(pid as i32, Ordering::SeqCst);

    let mut server = Server {
        port,
        socket,
        ca_pem: ca.pem().into_bytes(),
        public_key_pem: Vec::new(),
        has_native: false,
        has_sha256: false,
        version: String::new(),
        dir,
        bins,
    };
    let started = Instant::now();
    while !server.socket.exists() || !server.try_sql("SELECT 1") {
        assert!(
            started.elapsed() < Duration::from_secs(60),
            "MySQL did not start within 60 s:\n{}",
            server.log()
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    server.version = server.sql("SELECT VERSION()").trim().to_owned();
    server.public_key_pem = std::fs::read(data.join("public_key.pem")).unwrap_or_default();

    server.sql(&format!("CREATE DATABASE {DATABASE}"));
    for (user, password) in [SHA2_USER, RSA_USER, KEY_USER, FAST_USER] {
        server.sql(&format!(
            "CREATE USER '{user}'@'%' IDENTIFIED WITH caching_sha2_password BY '{password}'; \
             GRANT ALL ON {DATABASE}.* TO '{user}'@'%';"
        ));
    }
    server.sql(&format!(
        "CREATE USER '{0}'@'%' IDENTIFIED WITH caching_sha2_password BY '{1}' REQUIRE SSL; \
         GRANT ALL ON {DATABASE}.* TO '{0}'@'%';",
        TLS_USER.0, TLS_USER.1
    ));
    server.has_native = server.try_sql(&format!(
        "CREATE USER '{0}'@'%' IDENTIFIED WITH mysql_native_password BY '{1}'; \
         GRANT ALL ON {DATABASE}.* TO '{0}'@'%';",
        NATIVE_USER.0, NATIVE_USER.1
    ));
    server.has_sha256 = server.try_sql(&format!(
        "CREATE USER '{0}'@'%' IDENTIFIED WITH sha256_password BY '{1}'; \
         GRANT ALL ON {DATABASE}.* TO '{0}'@'%';",
        SHA256_USER.0, SHA256_USER.1
    ));
    eprintln!(
        "MySQL {} on port {port} (mysql_native_password: {}, sha256_password: {})",
        server.version, server.has_native, server.has_sha256
    );
    server
}
