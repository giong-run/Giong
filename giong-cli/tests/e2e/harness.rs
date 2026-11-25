//! Running the real `giong` binary the way a person or an agent does: in a
//! directory of its own, with a controlled environment, a timeout, and the
//! output checked against the contract in `src/report.rs`.
//!
//! Standard library only (and serde_json, which the crate already uses), so
//! the tests build wherever the command builds: Linux, macOS and Windows.

use std::ffi::OsString;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use serde_json::Value;

/// The binary under test, built by cargo for this test target.
pub const GIONG: &str = env!("CARGO_BIN_EXE_giong");

/// The version every JSON document must carry.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// How long a command may run before the test calls it hung. Commands that
/// are expected to be quick (everything but a build) must finish well inside.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(60);

/// A directory removed when the value is dropped (kept when
/// `GIONG_E2E_KEEP=1`, to look at after a failure).
pub struct TempDir {
    path: PathBuf,
}

impl TempDir {
    pub fn new(tag: &str) -> TempDir {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        let path =
            std::env::temp_dir().join(format!("giong-e2e-{}-{}-{tag}", std::process::id(), n));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).expect("create a temporary directory");
        // Canonical, so paths the command prints compare equal (macOS's
        // /var -> /private/var, Windows' short names).
        let path = std::fs::canonicalize(&path).unwrap_or(path);
        // Windows' canonical form is the verbatim `\\?\C:\...`, which no
        // command prints; keep the plain drive form.
        #[cfg(windows)]
        let path = match path.to_str().and_then(|s| s.strip_prefix(r"\\?\")) {
            Some(rest) if rest.as_bytes().get(1) == Some(&b':') => PathBuf::from(rest),
            _ => path,
        };
        TempDir { path }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn join(&self, p: impl AsRef<Path>) -> PathBuf {
        self.path.join(p)
    }

    /// Writes a file, making its directories.
    pub fn write(&self, rel: &str, text: &str) -> PathBuf {
        let p = self.path.join(rel);
        if let Some(d) = p.parent() {
            std::fs::create_dir_all(d).unwrap();
        }
        std::fs::write(&p, text).unwrap();
        p
    }

    pub fn read(&self, rel: &str) -> String {
        std::fs::read_to_string(self.path.join(rel)).unwrap_or_else(|e| panic!("read {rel}: {e}"))
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        if std::env::var_os("GIONG_E2E_KEEP").is_none() {
            let _ = std::fs::remove_dir_all(&self.path);
        } else {
            eprintln!("kept {}", self.path.display());
        }
    }
}

/// A `giong` invocation being set up.
pub struct Run {
    args: Vec<OsString>,
    dir: Option<PathBuf>,
    env: Vec<(OsString, Option<OsString>)>,
    timeout: Duration,
    stdin: Option<Vec<u8>>,
}

/// Starts setting up `giong <args>`.
pub fn giong<I, S>(args: I) -> Run
where
    I: IntoIterator<Item = S>,
    S: Into<OsString>,
{
    Run {
        args: args.into_iter().map(Into::into).collect(),
        dir: None,
        env: Vec::new(),
        timeout: DEFAULT_TIMEOUT,
        stdin: None,
    }
}

impl Run {
    pub fn arg(mut self, a: impl Into<OsString>) -> Run {
        self.args.push(a.into());
        self
    }

    pub fn dir(mut self, d: impl AsRef<Path>) -> Run {
        self.dir = Some(d.as_ref().to_path_buf());
        self
    }

    pub fn env(mut self, k: impl Into<OsString>, v: impl Into<OsString>) -> Run {
        self.env.push((k.into(), Some(v.into())));
        self
    }

    pub fn env_remove(mut self, k: impl Into<OsString>) -> Run {
        self.env.push((k.into(), None));
        self
    }

    pub fn timeout(mut self, t: Duration) -> Run {
        self.timeout = t;
        self
    }

    pub fn stdin(mut self, bytes: impl Into<Vec<u8>>) -> Run {
        self.stdin = Some(bytes.into());
        self
    }

    /// Runs it to the end (or the timeout) and gives what it printed.
    pub fn run(self) -> Out {
        let mut cmd = Command::new(GIONG);
        cmd.args(&self.args);
        // Cargo sets these for the test binary; a person's shell does not.
        // Tests that want them set them again.
        for k in [
            "CARGO",
            "RUSTC",
            "RUSTC_WRAPPER",
            "CARGO_TARGET_DIR",
            "RUSTFLAGS",
        ] {
            cmd.env_remove(k);
        }
        cmd.env("NO_COLOR", "1");
        for (k, v) in &self.env {
            match v {
                Some(v) => cmd.env(k, v),
                None => cmd.env_remove(k),
            };
        }
        if let Some(d) = &self.dir {
            cmd.current_dir(d);
        }
        run_with_timeout(cmd, self.stdin, self.timeout, &self.args)
    }
}

/// What a run printed and how it ended.
#[derive(Debug)]
pub struct Out {
    pub args: Vec<OsString>,
    /// `None` when it was killed at the timeout.
    pub status: Option<ExitStatus>,
    pub stdout: String,
    pub stderr: String,
    pub elapsed: Duration,
}

impl Out {
    /// The exit code; panics with the output when the command hung or was
    /// killed by a signal.
    pub fn code(&self) -> i32 {
        match self.status {
            None => panic!(
                "giong {} did not finish in time (killed after {:?})\n{}",
                self.cmdline(),
                self.elapsed,
                self.dump()
            ),
            Some(s) => s
                .code()
                .unwrap_or_else(|| panic!("giong {} died: {s:?}\n{}", self.cmdline(), self.dump())),
        }
    }

    pub fn cmdline(&self) -> String {
        self.args
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .join(" ")
    }

    pub fn dump(&self) -> String {
        format!(
            "--- stdout ---\n{}\n--- stderr ---\n{}",
            self.stdout, self.stderr
        )
    }

    /// Asserts the exit code.
    pub fn expect_code(&self, want: i32) -> &Out {
        let got = self.code();
        assert_eq!(
            got,
            want,
            "giong {}: exit code {got}, want {want}\n{}",
            self.cmdline(),
            self.dump()
        );
        self
    }

    /// The single JSON document on stdout, checked against the report
    /// contract for `command`, and the exit code checked against it.
    pub fn report(&self, command: &str) -> Value {
        let code = self.code();
        let v = crate::schema::parse_single_document(&self.stdout)
            .unwrap_or_else(|e| panic!("giong {}: {e}\n{}", self.cmdline(), self.dump()));
        if let Err(e) = crate::schema::check_report(&v, command) {
            panic!(
                "giong {}: the JSON does not follow the contract: {e}\n{}",
                self.cmdline(),
                self.dump()
            );
        }
        let want = crate::schema::exit_code_for(&v);
        assert_eq!(
            code,
            want,
            "giong {}: exit code {code} does not match the document (want {want})\n{}",
            self.cmdline(),
            self.dump()
        );
        v
    }
}

fn run_with_timeout(
    mut cmd: Command,
    stdin: Option<Vec<u8>>,
    timeout: Duration,
    args: &[OsString],
) -> Out {
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    cmd.stdin(if stdin.is_some() {
        Stdio::piped()
    } else {
        Stdio::null()
    });
    let start = Instant::now();
    let mut child = cmd
        .spawn()
        .unwrap_or_else(|e| panic!("cannot start {GIONG}: {e}"));
    if let Some(bytes) = stdin {
        let mut w = child.stdin.take().unwrap();
        std::thread::spawn(move || {
            use std::io::Write;
            let _ = w.write_all(&bytes);
        });
    }
    let reader = |mut r: Box<dyn Read + Send>| {
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            let _ = r.read_to_end(&mut buf);
            String::from_utf8_lossy(&buf).into_owned()
        })
    };
    let out_t = reader(Box::new(child.stdout.take().unwrap()));
    let err_t = reader(Box::new(child.stderr.take().unwrap()));
    let status = loop {
        match child.try_wait() {
            Ok(Some(s)) => break Some(s),
            Ok(None) if start.elapsed() > timeout => {
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(10)),
            Err(e) => panic!("waiting for giong: {e}"),
        }
    };
    let elapsed = start.elapsed();
    // After a kill, a grandchild (a fake tool that sleeps) may still hold the
    // pipes; do not wait for it forever.
    let join = |t: std::thread::JoinHandle<String>| -> String {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !t.is_finished() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        if t.is_finished() {
            t.join().unwrap_or_default()
        } else {
            "<output still open after the kill>".into()
        }
    };
    Out {
        args: args.to_vec(),
        status,
        stdout: join(out_t),
        stderr: join(err_t),
        elapsed,
    }
}

/// A fake program (`rustc`, `cargo`, `cc`...) in `dir`, written as a shell
/// script on Unix and a batch file on Windows. `unix` and `windows` are the
/// bodies. Returns the path to give in `RUSTC`/`CARGO`, or the directory to
/// put first on `PATH`.
pub fn fake_tool(dir: &Path, name: &str, unix: &str, windows: &str) -> PathBuf {
    std::fs::create_dir_all(dir).unwrap();
    if cfg!(windows) {
        let p = dir.join(format!("{name}.cmd"));
        std::fs::write(&p, format!("@echo off\r\n{windows}\r\n")).unwrap();
        p
    } else {
        // The script is written by `cp`, never through a file this process
        // holds open for writing: a child another test thread forks in the
        // meantime inherits that descriptor until it execs, and running the
        // script then fails with ETXTBSY ("Text file busy").
        let p = dir.join(name);
        let text = dir.join(format!(".{name}.txt"));
        std::fs::write(&text, format!("#!/bin/sh\n{unix}\n")).unwrap();
        let cp = Command::new("cp").arg(&text).arg(&p).status().expect("cp");
        assert!(cp.success(), "cp {} {}", text.display(), p.display());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        p
    }
}

/// `PATH` with `dir` first.
pub fn path_with(dir: &Path) -> OsString {
    let mut paths = vec![dir.to_path_buf()];
    if let Some(p) = std::env::var_os("PATH") {
        paths.extend(std::env::split_paths(&p));
    }
    std::env::join_paths(paths).unwrap()
}

/// A package with no dependencies to fetch, so the tests run offline.
pub fn new_package(dir: &TempDir, name: &str, extra_manifest: &str) -> PathBuf {
    dir.write(
        "Cargo.toml",
        &format!(
            "[package]\nname = \"{name}\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n\
             [workspace]\n{extra_manifest}"
        ),
    );
    dir.write("src/main.rs", "fn main() {\n    println!(\"hello\");\n}\n");
    dir.path().to_path_buf()
}

/// The commands this build of `giong` has, from `giong help --json`.
pub fn commands() -> Vec<String> {
    let out = giong(["help", "--json"]).run();
    let v = out.report("help");
    v["data"]["commands"]
        .as_array()
        .expect("help --json lists data.commands")
        .iter()
        .map(|c| c["name"].as_str().unwrap().to_string())
        .collect()
}

/// Whether this build has `name`; prints a note when it does not, so a
/// scenario for a command still being written shows as skipped in the log.
pub fn has_command(name: &str) -> bool {
    let has = commands().iter().any(|c| c == name);
    if !has {
        eprintln!("SKIP: this giong has no `{name}` command yet");
    }
    has
}

/// Slow tests (real builds of generated projects) run when `GIONG_E2E_SLOW=1`
/// (the cli-e2e workflow sets it).
pub fn slow_enabled(test: &str) -> bool {
    let on = std::env::var_os("GIONG_E2E_SLOW").is_some_and(|v| v != "0");
    if !on {
        eprintln!("SKIP: {test} is slow; run with GIONG_E2E_SLOW=1");
    }
    on
}

/// Finds files under `dir` whose text contains `needle`.
pub fn grep_files(dir: &Path, ext: &str, needle: &str) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&d) else {
            continue;
        };
        for e in rd.flatten() {
            let p = e.path();
            let name = e.file_name();
            if name == "target" || name == ".git" {
                continue;
            }
            if p.is_dir() {
                stack.push(p);
            } else if p.extension().is_some_and(|x| x == ext)
                && std::fs::read_to_string(&p).is_ok_and(|t| t.contains(needle))
            {
                out.push(p);
            }
        }
    }
    out.sort();
    out
}

/// Whether cargo and rustc run here. CI also runs this binary in a bare
/// container (ci.yml's docker job: no cargo, rustc or git), where the tests
/// that build a project skip with a note.
pub fn toolchain(test: &str) -> bool {
    static HAS: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    let has = *HAS.get_or_init(|| {
        ["cargo", "rustc"].iter().all(|t| {
            Command::new(t)
                .arg("--version")
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .is_ok_and(|s| s.success())
        })
    });
    if !has {
        eprintln!("SKIP: {test} needs cargo and rustc, which are not on PATH");
    }
    has
}
