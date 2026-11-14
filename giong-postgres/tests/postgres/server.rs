//! A throwaway PostgreSQL server for the tests: a fresh cluster in a
//! temporary directory, started once per test binary and killed when the
//! binary exits.
//!
//! The binaries are found through `GIONG_PG_BIN`, then in
//! `/usr/lib/postgresql/<version>/bin` (Debian, Ubuntu and the GitHub
//! runners), then on `PATH`. Without them the tests are skipped with a
//! message, unless `GIONG_PG_REQUIRED=1`, which makes that a failure (CI sets
//! it).

use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use giong_postgres::{Config, Tls, TlsConnector};

pub struct Server {
    pub port: u16,
    pub socket_dir: PathBuf,
    pub ca_pem: Vec<u8>,
    dir: PathBuf,
}

/// Passwords of the roles the server is set up with.
pub const SCRAM_USER: (&str, &str) = ("scram_user", "scram-secret");
pub const MD5_USER: (&str, &str) = ("md5_user", "md5-secret");
pub const CLEAR_USER: (&str, &str) = ("clear_user", "clear-secret");
pub const TLS_USER: (&str, &str) = ("tls_user", "tls-secret");

static SERVER: OnceLock<Option<Server>> = OnceLock::new();
static PID: AtomicI32 = AtomicI32::new(0);
static DIR: OnceLock<PathBuf> = OnceLock::new();

/// The server, or `None` (after saying why) when PostgreSQL is not installed.
pub fn server() -> Option<&'static Server> {
    SERVER
        .get_or_init(|| {
            let Some(bin) = find_bin() else {
                if std::env::var_os("GIONG_PG_REQUIRED").is_some_and(|v| v == "1") {
                    panic!("GIONG_PG_REQUIRED=1 but no PostgreSQL binaries were found; set GIONG_PG_BIN");
                }
                eprintln!("skipping: no PostgreSQL binaries found (set GIONG_PG_BIN)");
                return None;
            };
            Some(start(&bin))
        })
        .as_ref()
}

impl Server {
    /// The superuser over the Unix socket (trust authentication).
    pub fn local(&self) -> Config {
        Config::new()
            .host(self.socket_dir.to_str().unwrap())
            .port(self.port)
            .user("postgres")
            .dbname("postgres")
    }

    /// A user over TCP to 127.0.0.1, without TLS.
    pub fn tcp(&self, (user, password): (&str, &str)) -> Config {
        Config::new()
            .host("127.0.0.1")
            .port(self.port)
            .user(user)
            .password(password)
            .dbname("postgres")
    }

    /// A user over TCP with TLS, trusting the test authority.
    pub fn tls(&self, host: &str, (user, password): (&str, &str)) -> Config {
        Config::new()
            .host(host)
            .port(self.port)
            .user(user)
            .password(password)
            .dbname("postgres")
            .tls(Tls::Verify(
                TlsConnector::with_root_pem(&self.ca_pem).unwrap(),
            ))
    }

    /// Runs SQL as the superuser with psql.
    pub fn psql(&self, sql: &str) -> String {
        let out = self
            .command("psql")
            .args(["-X", "-q", "-A", "-t", "-v", "ON_ERROR_STOP=1", "-h"])
            .arg(&self.socket_dir)
            .args([
                "-p",
                &self.port.to_string(),
                "-U",
                "postgres",
                "-d",
                "postgres",
                "-c",
                sql,
            ])
            .output()
            .expect("run psql");
        assert!(
            out.status.success(),
            "psql failed on {sql:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout).unwrap()
    }

    fn command(&self, name: &str) -> Command {
        let bin = find_bin().expect("found before");
        let mut cmd = Command::new(bin.join(name));
        if let Some((uid, gid)) = run_as() {
            cmd.uid(uid).gid(gid);
        }
        cmd
    }

    pub fn log(&self) -> String {
        std::fs::read_to_string(self.dir.join("server.log")).unwrap_or_default()
    }
}

fn find_bin() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("GIONG_PG_BIN") {
        return Some(PathBuf::from(dir));
    }
    let mut versions: Vec<(u32, PathBuf)> = std::fs::read_dir("/usr/lib/postgresql")
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| {
            let v = e.file_name().to_str()?.parse().ok()?;
            let bin = e.path().join("bin");
            bin.join("initdb").exists().then_some((v, bin))
        })
        .collect();
    versions.sort();
    if let Some((_, bin)) = versions.pop() {
        return Some(bin);
    }
    std::env::var_os("PATH").and_then(|path| {
        std::env::split_paths(&path)
            .find(|dir| dir.join("initdb").exists() && dir.join("postgres").exists())
    })
}

/// PostgreSQL refuses to run as root: when the tests do, the server runs as
/// the `postgres` user.
pub fn run_as() -> Option<(u32, u32)> {
    // SAFETY: geteuid has no preconditions.
    if unsafe { libc::geteuid() } != 0 {
        return None;
    }
    let passwd = std::fs::read_to_string("/etc/passwd").ok()?;
    passwd.lines().find_map(|line| {
        let f: Vec<&str> = line.split(':').collect();
        (f.first() == Some(&"postgres")).then(|| (f[2].parse().unwrap(), f[3].parse().unwrap()))
    })
}

pub fn chown_tree(path: &Path, owner: Option<(u32, u32)>) {
    let Some((uid, gid)) = owner else { return };
    std::os::unix::fs::chown(path, Some(uid), Some(gid)).unwrap();
    if path.is_dir() {
        for entry in std::fs::read_dir(path).unwrap() {
            chown_tree(&entry.unwrap().path(), owner);
        }
    }
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

fn start(bin: &Path) -> Server {
    let owner = run_as();
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("giong-pg-{}-{stamp}", std::process::id()));
    let data = dir.join("data");
    let socket_dir = dir.join("socket");
    std::fs::create_dir_all(&socket_dir).unwrap();
    DIR.set(dir.clone()).unwrap();
    // SAFETY: registering a plain function to run at exit.
    unsafe { libc::atexit(stop_server) };
    chown_tree(&dir, owner);

    let mut initdb = Command::new(bin.join("initdb"));
    initdb.args(["-D"]).arg(&data).args([
        "-U",
        "postgres",
        "-A",
        "trust",
        "-E",
        "UTF8",
        "--locale=C",
        "--no-sync",
    ]);
    if let Some((uid, gid)) = owner {
        initdb.uid(uid).gid(gid);
    }
    let out = initdb.output().expect("run initdb");
    assert!(
        out.status.success(),
        "initdb failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    // A certificate authority and a server certificate for TLS.
    let ca_key = rcgen::KeyPair::generate().unwrap();
    let mut ca_params = rcgen::CertificateParams::new(Vec::<String>::new()).unwrap();
    ca_params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
    ca_params
        .distinguished_name
        .push(rcgen::DnType::CommonName, "giong-postgres test CA");
    let ca = ca_params.self_signed(&ca_key).unwrap();
    let server_key = rcgen::KeyPair::generate().unwrap();
    let server_params =
        rcgen::CertificateParams::new(vec!["localhost".to_owned(), "127.0.0.1".to_owned()])
            .unwrap();
    let server_cert = server_params.signed_by(&server_key, &ca, &ca_key).unwrap();
    std::fs::write(data.join("server.crt"), server_cert.pem()).unwrap();
    let key_path = data.join("server.key");
    std::fs::write(&key_path, server_key.serialize_pem()).unwrap();
    std::fs::set_permissions(&key_path, std::fs::Permissions::from_mode(0o600)).unwrap();

    let port = free_port();
    let mut conf = std::fs::OpenOptions::new()
        .append(true)
        .open(data.join("postgresql.conf"))
        .unwrap();
    writeln!(
        conf,
        "\nlisten_addresses = '127.0.0.1,127.0.0.2'\nport = {port}\nunix_socket_directories = '{}'\n\
         ssl = on\nssl_cert_file = 'server.crt'\nssl_key_file = 'server.key'\n\
         fsync = off\nsynchronous_commit = off\nfull_page_writes = off\n\
         max_connections = 200\npassword_encryption = 'scram-sha-256'\n\
         log_min_messages = warning\n",
        socket_dir.display()
    )
    .unwrap();
    std::fs::write(
        data.join("pg_hba.conf"),
        "local all all trust\n\
         host all postgres 127.0.0.0/8 trust\n\
         host all md5_user 127.0.0.0/8 md5\n\
         host all clear_user 127.0.0.0/8 password\n\
         hostssl all tls_user 127.0.0.0/8 scram-sha-256\n\
         hostnossl all tls_user 127.0.0.0/8 reject\n\
         host all all 127.0.0.0/8 scram-sha-256\n",
    )
    .unwrap();
    chown_tree(&dir, owner);

    // The server is started from a thread that never ends, with SIGKILL as
    // its parent-death signal: if this process dies without running its exit
    // handlers, the server goes too.
    let (tx, rx) = std::sync::mpsc::channel();
    let (bin, data2, log) = (bin.to_owned(), data.clone(), dir.join("server.log"));
    std::thread::spawn(move || {
        let log = std::fs::File::create(&log).unwrap();
        let mut cmd = Command::new(bin.join("postgres"));
        cmd.arg("-D")
            .arg(&data2)
            .stdin(Stdio::null())
            .stdout(log.try_clone().unwrap())
            .stderr(log);
        if let Some((uid, gid)) = owner {
            cmd.uid(uid).gid(gid);
        }
        // SAFETY: prctl is async-signal-safe and touches only this process.
        unsafe {
            cmd.pre_exec(|| {
                libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL);
                Ok(())
            });
        }
        let mut child = cmd.spawn().expect("start postgres");
        tx.send(child.id()).unwrap();
        // Blocks until the server is gone, keeping this thread (its parent)
        // alive until then.
        let _ = child.wait();
    });
    let pid = rx.recv().unwrap();
    PID.store(pid as i32, Ordering::SeqCst);

    let server = Server {
        port,
        socket_dir,
        ca_pem: ca.pem().into_bytes(),
        dir,
    };
    let socket = server.socket_dir.join(format!(".s.PGSQL.{port}"));
    let started = Instant::now();
    while !socket.exists() || !server.ready() {
        assert!(
            started.elapsed() < Duration::from_secs(30),
            "PostgreSQL did not start within 30 s:\n{}",
            server.log()
        );
        std::thread::sleep(Duration::from_millis(50));
    }

    server.psql(&format!(
        "CREATE ROLE {} LOGIN PASSWORD '{}'; \
         CREATE ROLE {} LOGIN PASSWORD '{}'; \
         CREATE ROLE {} LOGIN PASSWORD '{}';",
        SCRAM_USER.0, SCRAM_USER.1, CLEAR_USER.0, CLEAR_USER.1, TLS_USER.0, TLS_USER.1
    ));
    server.psql(&format!(
        "SET password_encryption = 'md5'; CREATE ROLE {} LOGIN PASSWORD '{}';",
        MD5_USER.0, MD5_USER.1
    ));
    server
}

impl Server {
    fn ready(&self) -> bool {
        self.command("pg_isready")
            .arg("-h")
            .arg(&self.socket_dir)
            .args(["-p", &self.port.to_string(), "-q"])
            .status()
            .is_ok_and(|s| s.success())
    }
}
