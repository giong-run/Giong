//! `giong bench` end to end: against a URL (fixed rate, max, failures,
//! refusals), and on a project it builds itself, with `--against` comparing
//! a commit with the working tree. The services are small std-only HTTP
//! servers, so the builds take a second.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use serde_json::Value;

/// The variables of the cargo running these tests that would change how
/// the test project builds: under `scripts/check.sh tsan` they ask for a
/// sanitizer and `-Zbuild-std`, which a plain `cargo build` cannot give.
const OUTER_BUILD: &[&str] = &[
    "RUSTFLAGS",
    "CARGO_ENCODED_RUSTFLAGS",
    "CARGO_BUILD_RUSTFLAGS",
    "RUSTDOCFLAGS",
    "CARGO_BUILD_TARGET",
    "CARGO_TARGET_DIR",
    "TSAN_OPTIONS",
];

/// Whether these programs run here; the tests that build a project skip
/// without them (CI runs the test binaries in a bare container too).
fn tools(names: &[&str]) -> bool {
    let missing: Vec<&str> = names
        .iter()
        .copied()
        .filter(|n| {
            !Command::new(n)
                .arg("--version")
                .output()
                .is_ok_and(|o| o.status.success())
        })
        .collect();
    if !missing.is_empty() {
        eprintln!("skipped: {} not found", missing.join(", "));
    }
    missing.is_empty()
}

fn giong(cwd: &Path, args: &[&str]) -> (i32, Value) {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_giong"));
    cmd.args(args)
        .arg("--json")
        .current_dir(cwd)
        .env("GIONG_HOME", home_of(cwd))
        .env_remove("WORKERS");
    for k in OUTER_BUILD {
        cmd.env_remove(k);
    }
    let out = cmd.output().unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    let v = serde_json::from_str(&text).unwrap_or_else(|e| {
        panic!(
            "not JSON ({e}): {text}\nstderr: {}",
            String::from_utf8_lossy(&out.stderr)
        )
    });
    (out.status.code().unwrap_or(-1), v)
}

/// The `GIONG_HOME` of a run in `cwd`: next to it, not in the user's.
fn home_of(cwd: &Path) -> PathBuf {
    PathBuf::from(format!("{}.home", cwd.display()))
}

/// An HTTP/1.1 keep-alive server: `/fail` answers 500, anything else 200
/// after `delay`.
fn serve(delay: Duration) -> u16 {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = l.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for s in l.incoming() {
            let Ok(mut s) = s else { return };
            std::thread::spawn(move || {
                let mut buf = [0u8; 4096];
                let mut got = Vec::new();
                loop {
                    while !got.windows(4).any(|w| w == b"\r\n\r\n") {
                        match s.read(&mut buf) {
                            Ok(0) | Err(_) => return,
                            Ok(n) => got.extend_from_slice(&buf[..n]),
                        }
                    }
                    let end = got.windows(4).position(|w| w == b"\r\n\r\n").unwrap() + 4;
                    let fail = got.starts_with(b"GET /fail ");
                    got.drain(..end);
                    std::thread::sleep(delay);
                    let answer: &[u8] = if fail {
                        b"HTTP/1.1 500 Internal Server Error\r\nContent-Length: 4\r\n\r\nboom"
                    } else {
                        b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok"
                    };
                    if s.write_all(answer).is_err() {
                        return;
                    }
                }
            });
        }
    });
    port
}

/// The finding with this code.
fn finding<'a>(v: &'a Value, code: &str) -> &'a Value {
    v["findings"]
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["code"] == code)
        .unwrap_or_else(|| panic!("no {code} in {v}"))
}

/// A port nothing listens on. A port the system handed out and took back
/// is no good: the other tests here run alongside and bind port 0, so one of
/// their servers can get it and answer the probe. This one is below the
/// system's ephemeral ports (from 32768 on Linux, 49152 on macOS and
/// Windows), which binding port 0 never gives.
fn closed_port() -> u16 {
    let pid = std::process::id() as usize;
    (0..2000)
        .map(|i| 20_000 + ((pid + i * 7) % 12_000) as u16)
        .find(|&p| TcpListener::bind(("127.0.0.1", p)).is_ok())
        .expect("a free port from 20000 to 31999")
}

fn scratch(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("giongclibench-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

#[test]
fn a_url_at_a_fixed_rate_and_as_fast_as_it_answers() {
    let port = serve(Duration::ZERO);
    let dir = scratch("url");
    let url = format!("http://127.0.0.1:{port}/health");
    let csv = dir.join("rounds.csv");
    let (code, v) = giong(
        &dir,
        &[
            "bench",
            &url,
            "--rate",
            "400",
            "--conns",
            "4",
            "--duration",
            "1",
            "--warmup",
            "0.2",
            "--rounds",
            "2",
            "--csv",
            csv.to_str().unwrap(),
        ],
    );
    assert_eq!(code, 0, "{v}");
    assert_eq!(v["data"]["mode"], "fixed");
    let rounds = v["data"]["rounds"].as_array().unwrap();
    assert_eq!(rounds.len(), 2);
    for r in rounds {
        // 400/s for 1 s: every request that was due, answered.
        let n = r["answered"].as_u64().unwrap();
        assert!((390..=410).contains(&n), "{r}");
        assert_eq!(r["failed"], 0);
        assert!(r["p99_us"].as_f64().unwrap() > 0.0);
    }
    let text = std::fs::read_to_string(&csv).unwrap();
    assert_eq!(text.lines().count(), 3, "{text}");
    assert!(text.starts_with("round,build,label,mode,rate,"));

    let (code, v) = giong(
        &dir,
        &[
            "bench",
            &url,
            "--rate",
            "max",
            "--conns",
            "2",
            "--duration",
            "0.5",
            "--warmup",
            "0",
        ],
    );
    assert_eq!(code, 0, "{v}");
    assert_eq!(v["data"]["mode"], "max");
    assert!(
        v["data"]["rounds"][0]["rps"].as_f64().unwrap() > 100.0,
        "{v}"
    );

    // auto: probes first, then measures at half of it.
    let (code, v) = giong(
        &dir,
        &[
            "bench",
            &url,
            "--conns",
            "2",
            "--duration",
            "0.5",
            "--warmup",
            "0",
        ],
    );
    assert_eq!(code, 0, "{v}");
    let probe = v["data"]["probe_rps"].as_f64().unwrap();
    let rate = v["data"]["rate"].as_f64().unwrap();
    assert!((rate - probe / 2.0).abs() < 1.0, "{v}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn failures_are_reported_and_refusals_are_coded() {
    let port = serve(Duration::ZERO);
    let dir = scratch("fail");
    // Half the requests answer 500: an error (more than 1%).
    let url = format!("http://127.0.0.1:{port}/");
    let (code, v) = giong(
        &dir,
        &[
            "bench",
            &url,
            "--path",
            "/ok,/fail",
            "--rate",
            "200",
            "--conns",
            "2",
            "--duration",
            "0.5",
            "--warmup",
            "0",
        ],
    );
    assert_eq!(code, 1, "{v}");
    assert_eq!(finding(&v, "GIONG-E5063")["severity"], "error");
    let r = &v["data"]["rounds"][0];
    assert_eq!(r["statuses"]["500"], r["statuses"]["200"]);

    // Nothing listens: the probe finds no answer.
    let closed = closed_port();
    let (code, v) = giong(&dir, &["bench", &format!("http://127.0.0.1:{closed}/")]);
    assert_eq!(code, 2, "{v}");
    assert_eq!(v["error"]["code"], "GIONG-E5060");
    // At a fixed rate, every request fails instead.
    let (code, v) = giong(
        &dir,
        &[
            "bench",
            &format!("http://127.0.0.1:{closed}/"),
            "--rate",
            "50",
            "--duration",
            "0.3",
            "--warmup",
            "0",
            "--timeout",
            "0.5",
        ],
    );
    assert_eq!(code, 1, "{v}");
    finding(&v, "GIONG-E5063");

    for (args, code_want) in [
        (vec!["bench", "https://127.0.0.1/"], "GIONG-E5060"),
        (vec!["bench", "ftp://x/"], "GIONG-E5060"),
        (vec!["bench", &url, "--rate", "fast"], "GIONG-E5000"),
        (vec!["bench", &url, "--conns", "0"], "GIONG-E5000"),
        (vec!["bench", &url, "--path", "nope"], "GIONG-E5000"),
        (vec!["bench", &url, "--against", "main"], "GIONG-E5000"),
        (vec!["bench", &url, "--max-regression", "5"], "GIONG-E5000"),
        (
            vec![
                "bench",
                "--against",
                "main",
                "--rounds",
                "2",
                "--max-regression",
                "5",
            ],
            "GIONG-E5000",
        ),
        (vec!["bench", &url, "--header", "bad"], "GIONG-E5000"),
        (vec!["bench", &url, "--duration", "0"], "GIONG-E5000"),
    ] {
        let (code, v) = giong(&dir, &args);
        assert_eq!(code, 2, "{args:?}: {v}");
        assert_eq!(v["error"]["code"], code_want, "{args:?}");
    }
    // Outside a project and without a URL there is nothing to measure.
    let (code, v) = giong(&dir, &["bench"]);
    assert_eq!(
        (code, v["error"]["code"].as_str()),
        (2, Some("GIONG-E5001")),
        "{v}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// A std-only service: reads LISTEN, answers every request after spinning
/// `work` iterations.
fn service_source(work: u64) -> String {
    format!(
        r#"use std::io::{{Read, Write}};
fn main() {{
    let addr = std::env::var("LISTEN").expect("LISTEN");
    if std::env::var("WORKERS").is_ok() {{ eprintln!("WORKERS leaked from .env"); std::process::exit(3); }}
    let l = std::net::TcpListener::bind(&addr).unwrap();
    for s in l.incoming() {{
        let mut s = s.unwrap();
        let _ = s.set_nodelay(true);
        std::thread::spawn(move || {{
            let mut buf = [0u8; 4096];
            let mut got = Vec::new();
            loop {{
                while !got.windows(4).any(|w| w == b"\r\n\r\n") {{
                    match s.read(&mut buf) {{ Ok(0) | Err(_) => return, Ok(n) => got.extend_from_slice(&buf[..n]) }}
                }}
                got.clear();
                let mut x = 1u64;
                for i in 0..{work}u64 {{ x = std::hint::black_box(x.wrapping_mul(6364136223846793005).wrapping_add(i)); }}
                let body = format!("{{}}", x % 10);
                let answer = format!("HTTP/1.1 200 OK\r\nContent-Length: {{}}\r\n\r\n{{}}", body.len(), body);
                let _ = s.write_all(answer.as_bytes());
            }}
        }});
    }}
}}
"#
    )
}

fn git(dir: &Path, args: &[&str]) {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn a_project_is_built_started_and_compared_with_a_commit() {
    if !tools(&["cargo", "git"]) {
        return;
    }
    let dir = scratch("proj");
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(
        dir.join("Cargo.toml"),
        "[package]\nname = \"benchsvc\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[workspace]\n",
    )
    .unwrap();
    std::fs::write(dir.join("src/main.rs"), service_source(1_000)).unwrap();
    std::fs::write(dir.join(".gitignore"), "target\n.giong/\n.env\n").unwrap();
    // A laptop setting giong bench must not pass on.
    std::fs::write(dir.join(".env"), "WORKERS=1\n").unwrap();
    git(&dir, &["init", "-q"]);
    git(&dir, &["add", "."]);
    git(
        &dir,
        &[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "commit",
            "-qm",
            "v1",
        ],
    );

    // One build, one round.
    let (code, v) = giong(
        &dir,
        &[
            "bench",
            "--rate",
            "200",
            "--conns",
            "2",
            "--duration",
            "0.5",
            "--warmup",
            "0.2",
        ],
    );
    assert_eq!(code, 0, "{v}");
    assert_eq!(v["data"]["rounds"][0]["build"], "service");
    assert_eq!(v["data"]["rounds"][0]["failed"], 0, "{v}");
    assert!(
        v["data"]["notes"]
            .to_string()
            .contains("WORKERS from .env is ignored"),
        "{v}"
    );
    assert!(dir.join(".giong/bench/service.log").exists());

    // The working tree does 300 times the work: slower in every round.
    std::fs::write(dir.join("src/main.rs"), service_source(300_000)).unwrap();
    let (code, v) = giong(
        &dir,
        &[
            "bench",
            "--against",
            "HEAD",
            "--rate",
            "max",
            "--conns",
            "2",
            "--rounds",
            "3",
            "--duration",
            "0.5",
            "--warmup",
            "0.2",
            "--max-regression",
            "10",
        ],
    );
    assert_eq!(code, 1, "{v}");
    let rounds = v["data"]["rounds"].as_array().unwrap();
    let order: Vec<&str> = rounds
        .iter()
        .map(|r| r["build"].as_str().unwrap())
        .collect();
    assert_eq!(
        order,
        ["base", "change", "change", "base", "base", "change"],
        "rounds alternate"
    );
    assert_eq!(v["data"]["compare"]["rps"]["every_round"], "worse", "{v}");
    finding(&v, "GIONG-E5064");
    let csv = v["data"]["csv"].as_str().unwrap();
    assert_eq!(std::fs::read_to_string(csv).unwrap().lines().count(), 7);
    // The worktree is gone again.
    let list = Command::new("git")
        .args(["worktree", "list"])
        .current_dir(&dir)
        .output()
        .unwrap();
    assert_eq!(String::from_utf8_lossy(&list.stdout).lines().count(), 1);
    // It was made under GIONG_HOME, never below the temporary directory,
    // where any user can put a .cargo/config.toml that the build would read.
    let worktrees = home_of(&dir).join("bench");
    assert!(worktrees.is_dir(), "{}", worktrees.display());
    assert_eq!(std::fs::read_dir(&worktrees).unwrap().count(), 0);

    // The same code twice: no regression, and the report says it is noise.
    git(&dir, &["checkout", "-q", "--", "src/main.rs"]);
    let (code, v) = giong(
        &dir,
        &[
            "bench",
            "--against",
            "HEAD",
            "--rate",
            "100",
            "--conns",
            "1",
            "--rounds",
            "1",
            "--duration",
            "0.3",
            "--warmup",
            "0",
        ],
    );
    assert_eq!(code, 0, "{v}");
    // One round a build: no "every round" verdict either way.
    assert_eq!(v["data"]["compare"]["p99"]["every_round"], "overlap", "{v}");
    // A Windows executable names the path of its .pdb, which differs
    // between the worktree's build and this one, so only elsewhere are the
    // two builds byte for byte the same.
    if !cfg!(windows) {
        assert!(
            v["data"]["notes"].to_string().contains("same binary"),
            "{v}"
        );
    }

    let (code, v) = giong(&dir, &["bench", "--against", "no-such-ref"]);
    assert_eq!(
        (code, v["error"]["code"].as_str()),
        (2, Some("GIONG-E5061")),
        "{v}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_service_that_exits_is_reported_with_its_log() {
    if !tools(&["cargo"]) {
        return;
    }
    let dir = scratch("exits");
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(
        dir.join("Cargo.toml"),
        "[package]\nname = \"exits\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[workspace]\n",
    )
    .unwrap();
    std::fs::write(
        dir.join("src/main.rs"),
        "fn main() { eprintln!(\"DATABASE_URL is not set\"); std::process::exit(1) }\n",
    )
    .unwrap();
    let (code, v) = giong(&dir, &["bench", "--rate", "10"]);
    assert_eq!(code, 2, "{v}");
    assert_eq!(v["error"]["code"], "GIONG-E5060");
    assert!(
        v["error"]["message"]
            .to_string()
            .contains("DATABASE_URL is not set"),
        "{v}"
    );
    let _ = std::fs::remove_dir_all(&dir);
    let _ = std::fs::remove_dir_all(home_of(&dir));
}
