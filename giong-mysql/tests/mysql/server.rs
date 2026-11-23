//! Throwaway MySQL or MariaDB servers for the tests: a fresh data directory
//! in a temporary directory, started once per test binary and killed when
//! the binary exits.
//!
//! `mysqld` (or `mariadbd`) is found through `GIONG_MYSQL_BIN` (the
//! directory that holds it, such as the `bin` of a MySQL or MariaDB
//! tarball), then at `/usr/sbin/mysqld` (Debian, Ubuntu and the GitHub
//! runners), then on `PATH`; the `mysql` (or `mariadb`) client next to it,
//! in the `bin` beside it, or on `PATH`. Without them the tests are skipped
//! with a message, unless `GIONG_MYSQL_REQUIRED=1`, which makes that a
//! failure (CI sets it).
//!
//! [`server`] is the one most tests use. [`odd`] starts a second one, set up
//! the way some production servers are and the defaults are not.
//!
//! MySQL and MariaDB on Windows listen on named pipes instead of Unix
//! sockets, which giong-mysql does not speak: there `root` and the other
//! "socket" logins go over TCP to 127.0.0.1, and the tests about the Unix
//! socket itself are Unix only.

#[cfg(target_os = "linux")]
#[cfg(target_os = "linux")]
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, Once, OnceLock};
use std::time::{Duration, Instant};

use giong_mysql::{Config, Tls, TlsConnector};

pub struct Server {
    pub port: u16,
    pub socket: PathBuf,
    /// The authority that signed the server's certificate, and its file.
    pub ca_pem: Vec<u8>,
    pub ca_file: PathBuf,
    /// The certificates are the ones MySQL made itself, which name no host.
    pub auto_certificates: bool,
    /// The server's RSA key for `caching_sha2_password`.
    pub public_key_pem: Vec<u8>,
    /// Whether the server has the `mysql_native_password` plugin (gone in
    /// MySQL 9).
    pub has_native: bool,
    /// Whether it has `sha256_password`.
    pub has_sha256: bool,
    /// Whether it has `caching_sha2_password` (MariaDB does not; its
    /// users log in with `mysql_native_password`).
    pub has_caching_sha2: bool,
    /// Whether it is MariaDB.
    pub mariadb: bool,
    pub version: String,
    dir: PathBuf,
    bins: Bins,
}

#[derive(Clone)]
struct Bins {
    mysqld: PathBuf,
    mysql: PathBuf,
    /// `mariadb-install-db`, for MariaDB, which has no
    /// `--initialize-insecure`.
    install_db: Option<PathBuf>,
    /// The server listens on one address only: MariaDB before 10.11, whose
    /// `bind-address` takes no list.
    one_address: bool,
}

/// How a server is set up.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Role {
    /// With the defaults.
    Main,
    /// See [`odd`].
    Odd,
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

static SERVER: OnceLock<Result<Option<Server>, String>> = OnceLock::new();
static ODD: OnceLock<Result<Server, String>> = OnceLock::new();
static ODD_TURN: Mutex<()> = Mutex::new(());
/// The servers started, and their directories, for [`stop_servers`].
static STARTED: Mutex<Vec<(i32, PathBuf)>> = Mutex::new(Vec::new());
static AT_EXIT: Once = Once::new();

/// The server, or `None` (after saying why) when MySQL is not installed.
pub fn server() -> Option<&'static Server> {
    let server = SERVER.get_or_init(|| {
        let Some(bins) = find_bins() else {
            if std::env::var_os("GIONG_MYSQL_REQUIRED").is_some_and(|v| v == "1") {
                panic!(
                    "GIONG_MYSQL_REQUIRED=1 but mysqld and mysql were not found; set \
                     GIONG_MYSQL_BIN"
                );
            }
            eprintln!("skipping: mysqld and mysql not found (set GIONG_MYSQL_BIN)");
            return Ok(None);
        };
        start_once(bins, Role::Main).map(Some)
    });
    match server {
        Ok(server) => server.as_ref(),
        Err(why) => panic!("{why}"),
    }
}

/// Starts a server, once: if it does not start, the test that asked first
/// says why with the server's log, and every later test fails at once with
/// the first line of it. Each would otherwise try in turn, a minute a test.
fn start_once(bins: Bins, role: Role) -> Result<Server, String> {
    std::panic::catch_unwind(|| start(bins, role)).map_err(|panic| {
        let why = panic
            .downcast_ref::<String>()
            .map(String::as_str)
            .or_else(|| panic.downcast_ref::<&str>().copied())
            .unwrap_or("it panicked");
        format!(
            "the test server did not start (the first test to fail has its log): {}",
            why.lines().next().unwrap_or_default()
        )
    })
}

/// A second server, started the first time it is asked for, with settings
/// some production servers have and the defaults do not:
///
/// - `init_connect` sets the character set to latin1, as servers do that
///   were set up when latin1 was the default (it runs after the handshake,
///   which asked for utf8mb4, and is not run for root);
/// - `innodb_rollback_on_timeout` is on, so a lock wait timeout rolls back
///   the whole transaction.
///
/// It has the same users as [`server`] and a server id of its own. Its tests
/// take turns (the guard), as some change its global settings.
pub fn odd(main: &Server) -> (&'static Server, MutexGuard<'static, ()>) {
    let turn = ODD_TURN.lock().unwrap_or_else(|e| e.into_inner());
    match ODD.get_or_init(|| start_once(main.bins.clone(), Role::Odd)) {
        Ok(server) => (server, turn),
        Err(why) => panic!("{why}"),
    }
}

impl Server {
    /// `root` over the Unix socket (no password); over TCP on Windows.
    pub fn local(&self) -> Config {
        self.local_host().user("root").database(DATABASE)
    }

    /// The server's Unix socket, or on Windows 127.0.0.1 and its port.
    pub fn local_host(&self) -> Config {
        if cfg!(windows) {
            Config::new().host("127.0.0.1").port(self.port)
        } else {
            Config::new().host(self.socket.to_str().unwrap())
        }
    }

    /// The `mysql` client's arguments for logging in as `root`: over the
    /// Unix socket, or on Windows over TCP.
    fn root_client(&self) -> Command {
        let mut cmd = Command::new(&self.bins.mysql);
        cmd.args(["--no-defaults", "-uroot", "-N", "-B"]);
        if cfg!(windows) {
            cmd.args(["--protocol=TCP", "-h127.0.0.1"])
                .arg(format!("-P{}", self.port));
        } else {
            cmd.arg("-S").arg(&self.socket);
        }
        cmd
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

    /// A user over the Unix socket; over TCP on Windows.
    pub fn socket(&self, (user, password): (&str, &str)) -> Config {
        self.local_host()
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
        let out = self
            .root_client()
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
        self.root_client()
            .args(["-e", sql])
            .stderr(Stdio::null())
            .stdout(Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
    }

    /// A session variable that lets a recursive CTE run long enough to be
    /// stopped ([`SLOW_SELECT`](crate::client)).
    pub fn deep_recursion(&self, config: Config) -> Config {
        if self.mariadb {
            config.session_variable("max_recursive_iterations", "4294967295")
        } else {
            config.session_variable("cte_max_recursion_depth", "4294967295")
        }
    }

    /// The error number for a statement stopped by the statement timeout.
    pub fn timeout_code(&self) -> u16 {
        if self.mariadb {
            1969
        } else {
            3024
        }
    }

    pub fn log(&self) -> String {
        std::fs::read_to_string(self.dir.join("server.log")).unwrap_or_default()
    }
}

fn find_bins() -> Option<Bins> {
    let path_dirs: Vec<PathBuf> = std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).collect())
        .unwrap_or_default();
    let names = [exe("mysqld"), exe("mariadbd")];
    let mysqld = match std::env::var_os("GIONG_MYSQL_BIN") {
        Some(dir) => names
            .iter()
            .map(|n| PathBuf::from(&dir).join(n))
            .find(|p| p.exists())?,
        None => std::iter::once(PathBuf::from("/usr/sbin"))
            .chain(path_dirs.iter().cloned())
            .flat_map(|d| names.iter().map(move |n| d.join(n)))
            .find(|p| p.exists())?,
    };
    let here = mysqld.parent()?.to_owned();
    // The `bin` beside an `sbin`, and `scripts` for MariaDB tarballs.
    let base = here
        .parent()
        .map(|p| p.to_owned())
        .unwrap_or_else(|| here.clone());
    let near: Vec<PathBuf> = [here, base.join("bin"), base.join("scripts")]
        .into_iter()
        .chain(path_dirs)
        .chain([PathBuf::from("/usr/bin")])
        .collect();
    let find = |names: &[&str]| {
        near.iter()
            .flat_map(|d| names.iter().map(move |n| d.join(exe(n))))
            .find(|p| p.exists())
    };
    let mysql = find(&["mysql", "mariadb"])?;
    let version = Command::new(&mysqld).arg("--version").output().ok()?;
    let version = String::from_utf8_lossy(&version.stdout).into_owned();
    let mariadb = version.contains("MariaDB");
    let install_db = if mariadb {
        Some(find(&["mariadb-install-db", "mysql_install_db"])?)
    } else {
        None
    };
    Some(Bins {
        mysqld,
        mysql,
        install_db,
        one_address: mariadb && version_number(&version) < (10, 11),
    })
}

/// The major and minor number in what `mysqld --version` prints
/// ("... Ver 10.6.23-MariaDB ..."), or `(0, 0)`.
fn version_number(version: &str) -> (u32, u32) {
    let mut numbers = version
        .split("Ver ")
        .nth(1)
        .unwrap_or_default()
        .split(|c: char| !c.is_ascii_digit())
        .map(|n| n.parse().unwrap_or(0));
    (numbers.next().unwrap_or(0), numbers.next().unwrap_or(0))
}

#[cfg(unix)]
fn is_root() -> bool {
    // SAFETY: geteuid has no preconditions.
    unsafe { libc::geteuid() == 0 }
}

/// Windows: the server runs as the tests' user.
#[cfg(windows)]
fn is_root() -> bool {
    false
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

extern "C" fn stop_servers() {
    let Ok(started) = STARTED.try_lock() else {
        return;
    };
    for (pid, dir) in started.iter() {
        if *pid > 0 {
            kill(*pid);
        }
        let _ = std::fs::remove_dir_all(dir);
    }
}

/// A TCP port free now on every one of `addrs`, from below the kernel's
/// ephemeral range, for a server this test starts.
///
/// The server cannot be told to pick a port itself and say which, so the
/// test picks one and the server binds it a moment later. A port from the
/// ephemeral range (what binding port 0 gives) can be taken in that moment
/// by any socket of the tests or of anything else on the machine: a
/// connect's source port, or another port-0 bind. The kernel hands out
/// ports below the range only on request. The same rule as giong-redis's
/// test server (#128).
fn server_port(addrs: &[&str]) -> u16 {
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
        if addrs
            .iter()
            .all(|addr| std::net::TcpListener::bind((*addr, port)).is_ok())
        {
            return port;
        }
    }
}

/// For a server that listens on one address only: a port free on 127.0.0.1
/// for it, with connections to the same port of 127.0.0.2 passed on to it,
/// so that the tests reach every server at both addresses.
fn free_port_with_second_address() -> u16 {
    let (port, listener) = (0..100)
        .find_map(|_| {
            let port = server_port(&["127.0.0.1", "127.0.0.2"]);
            let listener = std::net::TcpListener::bind(("127.0.0.2", port)).ok()?;
            Some((port, listener))
        })
        .expect("a port free on 127.0.0.1 and 127.0.0.2");
    std::thread::spawn(move || {
        for client in listener.incoming().flatten() {
            let Ok(server) = std::net::TcpStream::connect(("127.0.0.1", port)) else {
                continue;
            };
            let _ = (client.set_nodelay(true), server.set_nodelay(true));
            let (Ok(client_too), Ok(server_too)) = (client.try_clone(), server.try_clone()) else {
                continue;
            };
            for (mut from, to) in [(client, server_too), (server, client_too)] {
                std::thread::spawn(move || {
                    let _ = std::io::copy(&mut from, &mut &to);
                    let _ = to.shutdown(std::net::Shutdown::Write);
                });
            }
        }
    });
    port
}

/// How many ports `start` tries before it gives up.
const START_ATTEMPTS: usize = 5;

/// Waits until the server answers on its Unix socket; or, if it exits first,
/// gives what it logged from `log_from` on.
fn wait_until_ready(server: &Server, gone: &AtomicBool, log_from: usize) -> Result<(), String> {
    let started = Instant::now();
    // Windows has no socket file: the server is ready when it answers.
    while !(cfg!(windows) || server.socket.exists()) || !server.try_sql("SELECT 1") {
        if gone.load(Ordering::Acquire) {
            return Err(server.log().get(log_from..).unwrap_or_default().to_owned());
        }
        if started.elapsed() > Duration::from_secs(60) {
            // Say which side failed: the server, or the client that sets it
            // up (which cannot run when a shared library is missing, say).
            let client = server
                .root_client()
                .args(["-e", "SELECT 1"])
                .output()
                .map(|out| String::from_utf8_lossy(&out.stderr).into_owned())
                .unwrap_or_else(|e| e.to_string());
            panic!(
                "The server did not answer {} within 60 s.\nThe client says: {client}\nThe server's log:\n{}",
                server.bins.mysql.display(),
                server.log()
            );
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    Ok(())
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

/// The directory above the one `mysqld` is in: the root of a tarball, or
/// `/usr`.
fn base_dir(bins: &Bins) -> PathBuf {
    let here = bins.mysqld.parent().unwrap();
    here.parent().unwrap_or(here).to_owned()
}

/// MariaDB's options for where its files are, which a server outside the
/// place it was built for does not find by itself.
fn mariadb_dirs(bins: &Bins) -> Vec<String> {
    let base = base_dir(bins);
    let mut args = vec![format!("--basedir={}", base.display())];
    let share = ["share/mariadb", "share/mysql", "share"]
        .iter()
        .map(|d| base.join(d))
        .find(|d| d.join("english/errmsg.sys").exists());
    if let Some(share) = share {
        args.push(format!("--lc-messages-dir={}", share.display()));
    }
    let plugins = ["lib/mysql/plugin", "lib/plugin"]
        .iter()
        .map(|d| base.join(d))
        .find(|d| d.exists());
    if let Some(plugins) = plugins {
        args.push(format!("--plugin-dir={}", plugins.display()));
    }
    args
}

fn start(bins: Bins, role: Role) -> Server {
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos()
        % 1_000_000_000;
    let name = match role {
        Role::Main => "my",
        Role::Odd => "my2",
    };
    // Short: the socket's path must fit in 107 bytes.
    let dir = std::env::temp_dir().join(format!("giong-{name}-{}-{stamp}", std::process::id()));
    let data = dir.join("data");
    std::fs::create_dir_all(&dir).unwrap();
    STARTED.lock().unwrap().push((0, dir.clone()));
    // SAFETY: registering a plain function to run at exit.
    AT_EXIT.call_once(|| unsafe {
        libc::atexit(stop_servers);
    });
    let mariadb = bins.install_db.is_some();

    let out = match &bins.install_db {
        None => mysqld(&bins)
            .arg("--initialize-insecure")
            .arg(format!("--datadir={}", data.display()))
            .arg(format!("--log-error={}", dir.join("init.log").display()))
            .output()
            .expect("run mysqld --initialize-insecure"),
        // MariaDB's installer for Windows is a tool of its own, which takes
        // the data directory and sets `root` up without a password.
        Some(install_db) if cfg!(windows) => Command::new(install_db)
            .arg(format!("--datadir={}", data.display()))
            .output()
            .expect("run mariadb-install-db"),
        Some(install_db) => {
            let mut cmd = Command::new(install_db);
            cmd.arg("--no-defaults");
            if is_root() {
                cmd.arg("--user=root");
            }
            cmd.args(mariadb_dirs(&bins))
                .arg(format!("--datadir={}", data.display()))
                .args(["--auth-root-authentication-method=normal", "--skip-test-db"])
                .output()
                .expect("run mariadb-install-db")
        }
    };
    assert!(
        out.status.success(),
        "setting up the data directory failed: {}{}{}",
        String::from_utf8_lossy(&out.stdout),
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

    // The port is free a moment before the server binds it. A server another
    // test starts can take it in that moment, and mysqld then exits; a port
    // it could not bind is tried again with another.
    let mut attempt = 1;
    let mut server = loop {
        let port = if bins.one_address {
            free_port_with_second_address()
        } else {
            // It listens on both addresses itself.
            server_port(&["127.0.0.1", "127.0.0.2"])
        };
        let socket = dir.join("mysqld.sock");
        let log_from = std::fs::metadata(dir.join("server.log")).map_or(0, |m| m.len() as usize);

        // The server is started from a thread that never ends, with SIGKILL as
        // its parent-death signal: if this process dies without running its exit
        // handlers, the server goes too.
        let (tx, rx) = std::sync::mpsc::channel();
        let gone = Arc::new(AtomicBool::new(false));
        let mut cmd = mysqld(&bins);
        cmd.arg(format!("--datadir={}", data.display()))
            .arg(format!("--port={port}"))
            .arg(if bins.one_address {
                "--bind-address=127.0.0.1"
            } else {
                "--bind-address=127.0.0.1,127.0.0.2"
            })
            .arg(format!("--socket={}", socket.display()))
            .arg(format!("--pid-file={}", dir.join("mysqld.pid").display()))
            .arg(format!("--log-error={}", dir.join("server.log").display()))
            .args([
                "--skip-name-resolve",
                "--innodb-flush-log-at-trx-commit=0",
                "--innodb-doublewrite=0",
                "--max-connections=300",
                "--max-allowed-packet=64M",
                // MySQL 8.4 has the plugin off by default; 8.0 does not know the
                // option, which `loose` lets it ignore.
                "--loose-mysql-native-password=ON",
            ]);
        if mariadb {
            // Off by default on MariaDB; the tests look at sessions there.
            cmd.args(mariadb_dirs(&bins)).arg("--performance-schema=ON");
        } else {
            cmd.args(["--mysqlx=OFF", "--secure-file-priv=NULL", "--skip-log-bin"]);
        }
        // MySQL makes certificates of its own when given none, as most
        // installations use them: with no name in them, for the server's own
        // authority. MariaDB 10 makes none.
        let auto_certificates = role == Role::Odd && !mariadb;
        if !auto_certificates {
            cmd.arg(format!("--ssl-ca={}", dir.join("ca.pem").display()))
                .arg(format!(
                    "--ssl-cert={}",
                    dir.join("server-cert.pem").display()
                ))
                .arg(format!(
                    "--ssl-key={}",
                    dir.join("server-key.pem").display()
                ));
        }
        if role == Role::Odd {
            cmd.args([
                "--init-connect=SET NAMES latin1",
                "--innodb-rollback-on-timeout=ON",
            ]);
        }
        cmd.stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let server_gone = gone.clone();
        std::thread::spawn(move || {
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
            let mut child = cmd.spawn().expect("start mysqld");
            tx.send(child.id()).unwrap();
            // Blocks until the server is gone, keeping this thread (its parent)
            // alive until then.
            let _ = child.wait();
            server_gone.store(true, Ordering::Release);
        });
        let pid = rx.recv().unwrap();
        STARTED
            .lock()
            .unwrap()
            .iter_mut()
            .find(|(_, d)| *d == dir)
            .unwrap()
            .0 = pid as i32;

        let server = Server {
            port,
            socket,
            ca_pem: ca.pem().into_bytes(),
            ca_file: dir.join("ca.pem"),
            auto_certificates,
            public_key_pem: Vec::new(),
            has_native: false,
            has_sha256: false,
            has_caching_sha2: !mariadb,
            mariadb,
            version: String::new(),
            dir: dir.clone(),
            bins: bins.clone(),
        };
        match wait_until_ready(&server, &gone, log_from) {
            Ok(()) => break server,
            Err(log) if log.contains("Address already in use") && attempt < START_ATTEMPTS => {
                eprintln!("mysqld: port {port} was taken before it could listen; trying another");
                attempt += 1;
            }
            Err(log) => panic!("The server stopped while it was starting. Its log:\n{log}"),
        }
    };
    server.version = server.sql("SELECT VERSION()").trim().to_owned();
    if server.auto_certificates {
        server.ca_file = data.join("ca.pem");
        server.ca_pem = std::fs::read(&server.ca_file).expect("MySQL made its certificates");
    }
    server.public_key_pem = std::fs::read(data.join("public_key.pem")).unwrap_or_default();

    if mariadb {
        // Anonymous users would win over 'user'@'%' for logins from
        // localhost.
        server.sql("DELETE FROM mysql.global_priv WHERE User = ''; FLUSH PRIVILEGES");
    }
    server.sql(&format!("CREATE DATABASE {DATABASE}"));
    // MariaDB has no caching_sha2_password: there, these users have its
    // default, mysql_native_password.
    let sha2 = if mariadb {
        ""
    } else {
        "WITH caching_sha2_password"
    };
    for (user, password) in [SHA2_USER, RSA_USER, KEY_USER, FAST_USER] {
        server.sql(&format!(
            "CREATE USER '{user}'@'%' IDENTIFIED {sha2} BY '{password}'; \
             GRANT ALL ON {DATABASE}.* TO '{user}'@'%';"
        ));
    }
    server.sql(&format!(
        "CREATE USER '{0}'@'%' IDENTIFIED {sha2} BY '{1}' REQUIRE SSL; \
         GRANT ALL ON {DATABASE}.* TO '{0}'@'%';",
        TLS_USER.0, TLS_USER.1
    ));
    let native = if mariadb {
        ""
    } else {
        "WITH mysql_native_password"
    };
    server.has_native = server.try_sql(&format!(
        "CREATE USER '{0}'@'%' IDENTIFIED {native} BY '{1}'; \
         GRANT ALL ON {DATABASE}.* TO '{0}'@'%';",
        NATIVE_USER.0, NATIVE_USER.1
    ));
    server.has_sha256 = server.try_sql(&format!(
        "CREATE USER '{0}'@'%' IDENTIFIED WITH sha256_password BY '{1}'; \
         GRANT ALL ON {DATABASE}.* TO '{0}'@'%';",
        SHA256_USER.0, SHA256_USER.1
    ));
    eprintln!(
        "{} on port {} (mysql_native_password: {}, sha256_password: {})",
        server.version, server.port, server.has_native, server.has_sha256
    );
    server
}
