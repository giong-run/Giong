//! `giong image` end to end on a small std-only service: the archive is
//! reproducible, the refusals are coded, and (where Docker or Podman
//! answers) the image loads, runs as nonroot without a shell, and serves.
//! `GIONG_IMAGE_REQUIRED=1` turns a missing container runtime into a
//! failure.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

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

fn giong(cwd: &Path, args: &[&str], env: &[(&str, &str)]) -> (i32, Value) {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_giong"));
    cmd.args(args).arg("--json").current_dir(cwd);
    for k in OUTER_BUILD {
        cmd.env_remove(k);
    }
    for (k, v) in env {
        cmd.env(k, v);
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

const SERVICE: &str = r#"use std::io::{Read, Write};
fn main() {
    let addr = std::env::var("LISTEN").unwrap_or_else(|_| "0.0.0.0:3000".into());
    let l = std::net::TcpListener::bind(&addr).unwrap();
    for s in l.incoming() {
        let mut s = s.unwrap();
        let mut buf = [0u8; 4096];
        let _ = s.read(&mut buf);
        let page = std::fs::read_to_string("static/index.html").unwrap_or_default();
        let ca = std::path::Path::new("/etc/ssl/certs/ca-certificates.crt").exists();
        let body = format!("{} uid-file={} ca={ca}", page.trim(), std::fs::read_to_string("/etc/passwd").map(|p| p.contains("nonroot")).unwrap_or(false));
        let _ = s.write_all(format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes());
    }
}
"#;

struct Project {
    dir: PathBuf,
}

impl Project {
    fn new(tag: &str) -> Project {
        let name = format!("giongimg{tag}{}", std::process::id());
        let dir = std::env::temp_dir().join(&name);
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::create_dir_all(dir.join("static")).unwrap();
        std::fs::write(
            dir.join("Cargo.toml"),
            format!("[package]\nname = \"{name}\"\nversion = \"0.3.1\"\nedition = \"2021\"\n\n[workspace]\n"),
        )
        .unwrap();
        std::fs::write(dir.join("src/main.rs"), SERVICE).unwrap();
        std::fs::write(dir.join("static/index.html"), "hello from the image\n").unwrap();
        std::fs::write(dir.join(".env.example"), "# LISTEN=0.0.0.0:3000\n").unwrap();
        Project { dir }
    }
}

impl Drop for Project {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn runtime() -> Option<String> {
    ["docker", "podman"]
        .into_iter()
        .map(String::from)
        .find(|c| {
            Command::new(c)
                .args(["info", "--format", "{{.ServerVersion}}"])
                .output()
                .is_ok_and(|o| o.status.success())
        })
}

fn list(archive: &str) -> Vec<String> {
    let out = Command::new("tar").args(["-tf", archive]).output().unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(String::from)
        .collect()
}

#[test]
fn the_archive_is_reproducible_and_complete() {
    let p = Project::new("repro");
    let date = [("SOURCE_DATE_EPOCH", "1700000000")];
    let (code, a) = giong(&p.dir, &["image", "--add", "static"], &date);
    if cfg!(not(target_os = "linux")) && a["error"]["code"] == "GIONG-E5070" {
        // macOS and Windows build Linux binaries only with cargo zigbuild.
        eprintln!("skipped: this machine builds no Linux binary (cargo zigbuild is not installed)");
        return;
    }
    assert_eq!(code, 0, "{a}");
    assert_eq!(a["data"]["port"], 3000, "the port comes from .env.example");
    assert_eq!(a["data"]["created"], "2023-11-14T22:13:20Z");
    let file = a["data"]["file"].as_str().unwrap().to_string();
    let first = std::fs::read(&file).unwrap();
    let (_, b) = giong(&p.dir, &["image", "--add", "static"], &date);
    assert_eq!(
        a["data"]["digest"], b["data"]["digest"],
        "same source, same digest"
    );
    assert_eq!(first, std::fs::read(&file).unwrap(), "byte for byte");

    let names = list(&file);
    for n in ["oci-layout", "index.json", "manifest.json"] {
        assert!(names.iter().any(|x| x == n), "{n} in {names:?}");
    }
    assert_eq!(
        names
            .iter()
            .filter(|n| n.starts_with("blobs/sha256/") && !n.ends_with('/'))
            .count(),
        3
    );
    // The layer holds the binary, the added files, the user and /tmp.
    let layer_dir = std::env::temp_dir().join(format!("giongimg-layer-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&layer_dir);
    std::fs::create_dir_all(&layer_dir).unwrap();
    let ok = Command::new("tar")
        .arg("-xf")
        .arg(&file)
        .arg("-C")
        .arg(&layer_dir)
        .status()
        .unwrap();
    assert!(ok.success());
    let index: Value =
        serde_json::from_slice(&std::fs::read(layer_dir.join("index.json")).unwrap()).unwrap();
    let manifest_digest = index["manifests"][0]["digest"].as_str().unwrap();
    assert_eq!(manifest_digest, a["data"]["digest"]);
    let manifest: Value = serde_json::from_slice(
        &std::fs::read(layer_dir.join("blobs/sha256").join(&manifest_digest[7..])).unwrap(),
    )
    .unwrap();
    let layer = layer_dir
        .join("blobs/sha256")
        .join(&manifest["layers"][0]["digest"].as_str().unwrap()[7..]);
    let inside = list(layer.to_str().unwrap());
    let bin = format!("app/{}", p.dir.file_name().unwrap().to_string_lossy());
    for n in [
        bin.as_str(),
        "app/static/index.html",
        "etc/passwd",
        "etc/group",
        "tmp/",
        "home/nonroot/",
    ] {
        assert!(inside.iter().any(|x| x == n), "{n} in {inside:?}");
    }
    if a["data"]["static"] == false {
        assert!(inside.iter().any(|x| x.contains("libc.so")), "{inside:?}");
    }
    let _ = std::fs::remove_dir_all(&layer_dir);
    // Another date, another digest.
    let (_, c) = giong(
        &p.dir,
        &["image", "--add", "static"],
        &[("SOURCE_DATE_EPOCH", "1700000001")],
    );
    assert_ne!(a["data"]["digest"], c["data"]["digest"]);
}

#[test]
fn refusals_are_coded() {
    let p = Project::new("bad");
    for (args, want) in [
        (vec!["image", "--tag", "Bad:Name"], "GIONG-E5076"),
        (vec!["image", "--push", "svc@sha256:00"], "GIONG-E5076"),
        (vec!["image", "--add", "../etc"], "GIONG-E5073"),
        (vec!["image", "--add", "/etc"], "GIONG-E5073"),
        (vec!["image", "--add", "nope"], "GIONG-E5073"),
        (
            vec!["image", "--target", "x86_64-apple-darwin"],
            "GIONG-E5070",
        ),
        (vec!["image", "--port", "0"], "GIONG-E5000"),
    ] {
        let (code, v) = giong(&p.dir, &args, &[]);
        assert_eq!(code, 2, "{args:?}: {v}");
        assert_eq!(v["error"]["code"], want, "{args:?}");
    }
    let (code, v) = giong(&std::env::temp_dir(), &["image"], &[]);
    assert_eq!(
        (code, v["error"]["code"].as_str()),
        (2, Some("GIONG-E5001")),
        "{v}"
    );
}

fn get(port: u16) -> Option<String> {
    let mut s = TcpStream::connect(("127.0.0.1", port)).ok()?;
    s.set_read_timeout(Some(Duration::from_secs(5))).ok()?;
    s.write_all(b"GET / HTTP/1.1\r\nHost: x\r\n\r\n").ok()?;
    let mut out = String::new();
    s.read_to_string(&mut out).ok()?;
    Some(out)
}

/// Builds with `extra` arguments, loads, runs and asks the image.
fn load_and_run(rt: &str, p: &Project, tag: &str, extra: &[&str]) -> Value {
    let mut args = vec!["image", "--add", "static", "--load", "--tag", tag];
    args.extend_from_slice(extra);
    let (code, v) = giong(&p.dir, &args, &[]);
    assert_eq!(code, 0, "{v}");
    let name = format!("giongimg-run-{}", std::process::id());
    let _ = Command::new(rt).args(["rm", "-f", &name]).output();
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let out = Command::new(rt)
        .args([
            "run",
            "-d",
            "--name",
            &name,
            "-p",
            &format!("127.0.0.1:{port}:3000"),
            tag,
        ])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut answer = None;
    while Instant::now() < deadline {
        if let Some(a) = get(port).filter(|a| a.contains("200 OK")) {
            answer = Some(a);
            break;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    let user = Command::new(rt)
        .args(["inspect", &name, "--format", "{{.Config.User}}"])
        .output()
        .unwrap();
    let shell = Command::new(rt)
        .args(["exec", &name, "sh", "-c", "true"])
        .output()
        .unwrap();
    let logs = Command::new(rt).args(["logs", &name]).output().unwrap();
    let _ = Command::new(rt).args(["rm", "-f", &name]).output();
    let _ = Command::new(rt).args(["image", "rm", "-f", tag]).output();
    let answer = answer
        .unwrap_or_else(|| panic!("no answer; logs: {}", String::from_utf8_lossy(&logs.stderr)));
    assert!(
        answer.contains("hello from the image uid-file=true ca=true"),
        "{answer}"
    );
    assert_eq!(String::from_utf8_lossy(&user.stdout).trim(), "65532:65532");
    assert!(!shell.status.success(), "the image has no shell");
    v
}

#[test]
fn the_image_loads_runs_as_nonroot_and_serves() {
    let Some(rt) = runtime() else {
        assert!(
            std::env::var("GIONG_IMAGE_REQUIRED").is_err(),
            "GIONG_IMAGE_REQUIRED is set and neither Docker nor Podman answers"
        );
        eprintln!("skipped: neither Docker nor Podman answers");
        return;
    };
    let p = Project::new("run");
    let id = std::process::id();
    // The default (static with musl when installed), then glibc with this
    // machine's libraries.
    load_and_run(&rt, &p, &format!("giongimg-test-a:{id}"), &[]);
    if cfg!(all(target_os = "linux", target_arch = "x86_64")) {
        let v = load_and_run(
            &rt,
            &p,
            &format!("giongimg-test-b:{id}"),
            &["--target", "x86_64-unknown-linux-gnu"],
        );
        assert_eq!(v["data"]["static"], false, "{v}");
        assert!(v["data"]["libraries"].as_array().unwrap().len() >= 2, "{v}");
    }
}
