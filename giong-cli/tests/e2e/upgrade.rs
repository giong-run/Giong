//! `giong upgrade` and the release checks of `giong doctor`, against a
//! registry of fake giong crates on disk (cargo's directory source replacing
//! crates.io), so nothing here needs the network. `GIONG_LATEST` says which
//! release is the newest, as it does for a mirror.

use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::harness::{giong, toolchain, Run, TempDir, VERSION};

/// A crate version in the fake registry: `giong` (with `src` as its lib.rs)
/// or `giong-web` (which depends on giong of its own version).
fn publish(reg: &Path, name: &str, version: &str, lib: &str, rust_version: &str) {
    let dir = reg.join(format!("{name}-{version}"));
    std::fs::create_dir_all(dir.join("src")).unwrap();
    let deps = if name == "giong" {
        String::new()
    } else {
        format!("\n[dependencies]\ngiong = \"{version}\"\n")
    };
    std::fs::write(
        dir.join("Cargo.toml"),
        format!(
            "[package]\nname = \"{name}\"\nversion = \"{version}\"\nedition = \"2021\"\n\
             rust-version = \"{rust_version}\"\n{deps}"
        ),
    )
    .unwrap();
    std::fs::write(dir.join("src/lib.rs"), lib).unwrap();
    std::fs::write(dir.join(".cargo-checksum.json"), "{\"files\":{}}").unwrap();
}

/// Both crates at `version`.
fn release(reg: &Path, version: &str, lib: &str, rust_version: &str) {
    publish(reg, "giong", version, lib, rust_version);
    publish(
        reg,
        "giong-web",
        version,
        "pub use giong::*;\n",
        rust_version,
    );
}

const OLD: &str = "1.4.2";

/// A project on giong 1.4.2 from the fake registry, which then gets this
/// command's release too.
struct Fixture {
    _dir: TempDir,
    root: PathBuf,
    home: PathBuf,
    reg: PathBuf,
}

impl Fixture {
    fn new(tag: &str, manifest_deps: &str, main: &str) -> Fixture {
        let dir = TempDir::new(&format!("upgrade-{tag}"));
        let reg = dir.join("registry");
        std::fs::create_dir_all(&reg).unwrap();
        release(
            &reg,
            OLD,
            "pub fn old_api() {}\npub const V: &str = \"old\";\n",
            "1.70",
        );
        let root = dir.join("app");
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::create_dir_all(root.join(".cargo")).unwrap();
        std::fs::write(
            root.join(".cargo/config.toml"),
            format!(
                "[source.crates-io]\nreplace-with = \"fake\"\n\n[source.fake]\ndirectory = {:?}\n",
                reg.display().to_string()
            ),
        )
        .unwrap();
        std::fs::write(
            root.join("Cargo.toml"),
            format!(
                "[package]\nname = \"app\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n\
                 [workspace]\n\n{manifest_deps}"
            ),
        )
        .unwrap();
        std::fs::write(root.join("src/main.rs"), main).unwrap();
        let home = dir.join("home");
        std::fs::create_dir_all(&home).unwrap();
        let f = Fixture {
            _dir: dir,
            root,
            home,
            reg,
        };
        f.cargo(&["generate-lockfile", "--offline"]);
        f
    }

    fn cargo(&self, args: &[&str]) {
        let out = std::process::Command::new("cargo")
            .args(args)
            .current_dir(&self.root)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "cargo {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    fn giong(&self, args: &[&str]) -> Run {
        giong(args.to_vec())
            .dir(&self.root)
            .env("GIONG_HOME", &self.home)
            .env_remove("GIONG_LATEST")
            .env_remove("GIONG_MCP_TOOL")
            .env_remove("GIONG_UPGRADE_CLI_FROM")
    }

    fn read(&self, rel: &str) -> String {
        std::fs::read_to_string(self.root.join(rel)).unwrap()
    }

    fn locked(&self) -> Vec<(String, String)> {
        let lock = self.read("Cargo.lock");
        let mut out = Vec::new();
        let mut name = None;
        for line in lock.lines() {
            if let Some(n) = line.strip_prefix("name = ") {
                name = Some(n.trim_matches('"').to_string());
            } else if let Some(v) = line.strip_prefix("version = ") {
                if let Some(n) = name.take().filter(|n| n.starts_with("giong")) {
                    out.push((n, v.trim_matches('"').to_string()));
                }
            }
        }
        out.sort();
        out
    }
}

fn codes(v: &Value) -> Vec<String> {
    v["findings"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f["code"].as_str().unwrap().to_string())
        .collect()
}

const DEPS: &str = "[dependencies]\n\
                    giong-web = \"1.4.2\" # the router\n\n\
                    [dev-dependencies]\n\
                    rt = { package = \"giong\", version = \"1.4\" }\n";

const MAIN: &str = "fn main() {\n    println!(\"{}\", giong_web::V);\n}\n";

#[test]
fn every_giong_crate_moves_and_the_build_is_checked() {
    if !toolchain("every_giong_crate_moves_and_the_build_is_checked") {
        return;
    }
    let f = Fixture::new("moves", DEPS, MAIN);
    release(&f.reg, VERSION, "pub const V: &str = \"new\";\n", "1.70");
    let manifest = f.read("Cargo.toml");
    let lock = f.read("Cargo.lock");

    // The doctor sees it first.
    let v = f
        .giong(&["doctor", "--json"])
        .env("GIONG_LATEST", VERSION)
        .run()
        .report("doctor");
    assert!(codes(&v).contains(&"GIONG-E5090".into()), "{v:#}");
    assert_eq!(v["data"]["giong"]["locked"]["giong-web"], OLD, "{v:#}");
    assert_eq!(v["data"]["giong"]["latest"], VERSION, "{v:#}");

    let out = f
        .giong(&["upgrade", "--dry-run", "--json"])
        .env("GIONG_LATEST", VERSION)
        .run();
    out.expect_code(0);
    let v = out.report("upgrade");
    assert_eq!(v["data"]["to"], VERSION);
    assert_eq!(v["data"]["from"], OLD);
    assert_eq!(v["data"]["edits"].as_array().unwrap().len(), 2, "{v:#}");
    assert_eq!(
        f.read("Cargo.toml"),
        manifest,
        "--dry-run changed Cargo.toml"
    );
    assert_eq!(f.read("Cargo.lock"), lock, "--dry-run changed Cargo.lock");

    let out = f
        .giong(&["upgrade", "--json"])
        .env("GIONG_LATEST", VERSION)
        .run();
    out.expect_code(0);
    let v = out.report("upgrade");
    assert_eq!(v["data"]["rolled_back"], false, "{v:#}");
    let (major, minor) = {
        let mut p = VERSION.split('.');
        (p.next().unwrap(), p.next().unwrap())
    };
    let want = manifest
        .replace(
            "giong-web = \"1.4.2\"",
            &format!("giong-web = \"{VERSION}\""),
        )
        .replace(
            "version = \"1.4\"",
            &format!("version = \"{major}.{minor}\""),
        );
    assert_eq!(f.read("Cargo.toml"), want, "only the versions change");
    assert_eq!(
        f.locked(),
        [
            ("giong".to_string(), VERSION.to_string()),
            ("giong-web".to_string(), VERSION.to_string())
        ]
    );
    assert!(!f.root.join(".giong").exists(), "the copies are removed");

    // Again: nothing to do.
    let v = f
        .giong(&["upgrade", "--json"])
        .env("GIONG_LATEST", VERSION)
        .run()
        .report("upgrade");
    assert_eq!(v["data"]["edits"].as_array().unwrap().len(), 0, "{v:#}");
    assert_eq!(f.read("Cargo.toml"), want);
}

#[test]
fn a_build_that_breaks_puts_every_file_back() {
    if !toolchain("a_build_that_breaks_puts_every_file_back") {
        return;
    }
    let main = "fn main() {\n    giong_web::old_api();\n}\n";
    let f = Fixture::new("breaks", DEPS, main);
    release(&f.reg, VERSION, "pub const V: &str = \"new\";\n", "1.70");
    let manifest = f.read("Cargo.toml");
    let lock = f.read("Cargo.lock");
    let out = f
        .giong(&["upgrade", "--json"])
        .env("GIONG_LATEST", VERSION)
        .run();
    out.expect_code(1);
    let v = out.report("upgrade");
    assert_eq!(v["data"]["rolled_back"], true, "{v:#}");
    assert!(
        !codes(&v).is_empty(),
        "the compiler's error is reported: {v:#}"
    );
    assert_eq!(f.read("Cargo.toml"), manifest);
    assert_eq!(f.read("Cargo.lock"), lock);
    assert!(!f.root.join(".giong").exists());
}

#[test]
fn a_release_that_needs_a_newer_rust_is_refused() {
    if !toolchain("a_release_that_needs_a_newer_rust_is_refused") {
        return;
    }
    let f = Fixture::new("msrv", DEPS, MAIN);
    release(&f.reg, VERSION, "pub const V: &str = \"new\";\n", "1.999");
    let manifest = f.read("Cargo.toml");
    let lock = f.read("Cargo.lock");
    let out = f
        .giong(&["upgrade", "--json"])
        .env("GIONG_LATEST", VERSION)
        .run();
    out.expect_code(2);
    let v = out.report("upgrade");
    assert_eq!(v["error"]["code"], "GIONG-E5003", "{v:#}");
    assert_eq!(f.read("Cargo.toml"), manifest);
    assert_eq!(f.read("Cargo.lock"), lock);
}

#[test]
fn a_project_that_promises_an_older_rust_is_not_moved() {
    if !toolchain("a_project_that_promises_an_older_rust_is_not_moved") {
        return;
    }
    let f = Fixture::new("promise", DEPS, MAIN);
    release(&f.reg, VERSION, "pub const V: &str = \"new\";\n", "1.80");
    let manifest = f.read("Cargo.toml").replace(
        "edition = \"2021\"",
        "edition = \"2021\"\nrust-version = \"1.75\"",
    );
    std::fs::write(f.root.join("Cargo.toml"), &manifest).unwrap();
    let out = f
        .giong(&["upgrade", "--json"])
        .env("GIONG_LATEST", VERSION)
        .run();
    out.expect_code(2);
    let v = out.report("upgrade");
    assert_eq!(v["error"]["code"], "GIONG-E5094", "{v:#}");
    assert_eq!(f.read("Cargo.toml"), manifest);
}

#[test]
fn a_pinned_giong_is_refused_and_nothing_changes() {
    if !toolchain("a_pinned_giong_is_refused_and_nothing_changes") {
        return;
    }
    let deps = "[dependencies]\ngiong-web = \"=1.4.2\"\n";
    let f = Fixture::new("pinned", deps, MAIN);
    release(&f.reg, VERSION, "pub const V: &str = \"new\";\n", "1.70");
    let manifest = f.read("Cargo.toml");
    let out = f
        .giong(&["upgrade", "--json"])
        .env("GIONG_LATEST", VERSION)
        .run();
    out.expect_code(2);
    let v = out.report("upgrade");
    assert_eq!(v["error"]["code"], "GIONG-E5095", "{v:#}");
    assert!(
        v["error"]["message"]
            .as_str()
            .unwrap()
            .contains("Cargo.toml:9"),
        "{v:#}"
    );
    assert_eq!(f.read("Cargo.toml"), manifest);
}

#[test]
fn offline_moves_to_this_commands_release() {
    if !toolchain("offline_moves_to_this_commands_release") {
        return;
    }
    let f = Fixture::new("offline", DEPS, MAIN);
    release(&f.reg, VERSION, "pub const V: &str = \"new\";\n", "1.70");
    // A newer release in the registry is not taken: --offline asks nobody.
    let v = f
        .giong(&["upgrade", "--offline", "--json"])
        .run()
        .report("upgrade");
    assert_eq!(v["data"]["to"], VERSION, "{v:#}");
    assert!(v.get("error").is_none(), "{v:#}");
    assert!(f
        .read("Cargo.toml")
        .contains(&format!("giong-web = \"{VERSION}\"")));
}

#[test]
fn versions_that_are_not_releases_to_move_to_are_refused() {
    if !toolchain("versions_that_are_not_releases_to_move_to_are_refused") {
        return;
    }
    let f = Fixture::new("to", DEPS, MAIN);
    for (to, code) in [
        ("2.0.0", "GIONG-E5093"),
        ("1.0.0", "GIONG-E5093"),
        ("soon", "GIONG-E5093"),
    ] {
        let out = f.giong(&["upgrade", "--to", to, "--json"]).run();
        out.expect_code(2);
        assert_eq!(out.report("upgrade")["error"]["code"], code, "--to {to}");
    }
    // The newest release unknown (here: a GIONG_LATEST that is not a
    // version), and not told to stay offline.
    let out = f
        .giong(&["upgrade", "--json"])
        .env("GIONG_LATEST", "soon")
        .run();
    out.expect_code(2);
    assert_eq!(out.report("upgrade")["error"]["code"], "GIONG-E5097");
}

#[test]
fn the_doctor_reports_mixed_releases_offline_too() {
    if !toolchain("the_doctor_reports_mixed_releases_offline_too") {
        return;
    }
    let deps = "[dependencies]\ngiong = \"1.4.2\"\ngiong-web = \"1.4.2\"\n";
    let f = Fixture::new("mixed", deps, MAIN);
    release(&f.reg, VERSION, "pub const V: &str = \"new\";\n", "1.70");
    // Only giong moves: giong-web stays on 1.4.2.
    f.cargo(&["update", "--offline", "-p", "giong"]);
    let v = f
        .giong(&["doctor", "--offline", "--json"])
        .run()
        .report("doctor");
    let c = codes(&v);
    assert!(c.contains(&"GIONG-E5091".into()), "{v:#}");
    assert!(
        !c.contains(&"GIONG-E5090".into()),
        "--offline asks nobody: {v:#}"
    );
    assert!(v["data"]["giong"]["latest"].is_null(), "{v:#}");
    let e5091 = v["findings"]
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["code"] == "GIONG-E5091")
        .unwrap();
    assert_eq!(e5091["file"], "Cargo.lock");

    let v = f
        .giong(&["doctor", "--json"])
        .env("GIONG_LATEST", "1.99.0")
        .run()
        .report("doctor");
    let c = codes(&v);
    assert!(c.contains(&"GIONG-E5090".into()), "{v:#}");
    assert!(c.contains(&"GIONG-E5092".into()), "{v:#}");
    assert_eq!(v["data"]["giong"]["latest_from"], "GIONG_LATEST");
}

#[test]
fn a_project_without_giong_from_a_registry_is_not_moved() {
    if !toolchain("a_project_without_giong_from_a_registry_is_not_moved") {
        return;
    }
    let dir = TempDir::new("upgrade-none");
    crate::harness::new_package(&dir, "plain", "");
    let out = giong(["upgrade", "--json"]).dir(dir.path()).run();
    out.expect_code(2);
    assert_eq!(out.report("upgrade")["error"]["code"], "GIONG-E5039");
}

#[test]
fn a_command_installed_by_neither_way_is_not_replaced() {
    let home = TempDir::new("upgrade-self-unknown");
    let run = |args: &[&str]| {
        giong(args.to_vec())
            .env("HOME", home.path())
            .env("USERPROFILE", home.path())
            .env("CARGO_HOME", home.join("cargo"))
            .env_remove("CARGO_INSTALL_ROOT")
            .env_remove("GIONG_INSTALL")
            .run()
    };
    let out = run(&["upgrade", "--self", "--to", "1.99.0", "--json"]);
    out.expect_code(2);
    assert_eq!(out.report("upgrade")["error"]["code"], "GIONG-E5096");
    let out = run(&["upgrade", "--self", "--to", "1.99.0", "--dry-run", "--json"]);
    out.expect_code(1);
    let v = out.report("upgrade");
    assert_eq!(v["data"]["install"]["method"], "unknown", "{v:#}");
    assert!(codes(&v).contains(&"GIONG-E5096".into()));
    let out = run(&["upgrade", "--self", "--offline", "--json"]);
    out.expect_code(2);
    assert_eq!(out.report("upgrade")["error"]["code"], "GIONG-E5000");
}

/// The published targets, as the install scripts name them.
#[cfg(unix)]
fn triple() -> Option<&'static str> {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("linux", "x86_64") => Some("x86_64-unknown-linux-musl"),
        ("linux", "aarch64") => Some("aarch64-unknown-linux-musl"),
        ("macos", "x86_64") => Some("x86_64-apple-darwin"),
        ("macos", "aarch64") => Some("aarch64-apple-darwin"),
        _ => None,
    }
}

/// Serves the files of `dir` over plain HTTP on loopback until the test ends.
#[cfg(unix)]
fn serve(dir: PathBuf) -> String {
    use std::io::{BufRead, BufReader, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let mut reader = BufReader::new(&stream);
            let mut first = String::new();
            let _ = reader.read_line(&mut first);
            loop {
                let mut l = String::new();
                if reader.read_line(&mut l).unwrap_or(0) <= 2 {
                    break;
                }
            }
            let path = first.split_whitespace().nth(1).unwrap_or("/");
            let mut stream = &stream;
            match std::fs::read(dir.join(path.trim_start_matches('/'))) {
                Ok(body) => {
                    let _ = write!(
                        stream,
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    );
                    let _ = stream.write_all(&body);
                }
                Err(_) => {
                    let _ = write!(
                        stream,
                        "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                    );
                }
            }
        }
    });
    format!("http://{addr}")
}

#[cfg(unix)]
fn has(tool: &str) -> bool {
    std::process::Command::new(tool)
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

#[cfg(unix)]
#[test]
fn the_install_scripts_command_is_replaced_by_the_release_archive() {
    use std::os::unix::fs::PermissionsExt;
    let Some(triple) = triple() else { return };
    if !has("curl") || !has("tar") || !has("sha256sum") {
        eprintln!("SKIP: needs curl, tar and sha256sum");
        return;
    }
    let dir = TempDir::new("upgrade-self-script");
    // The installed command: a copy of this build.
    let bin = dir.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let exe = bin.join("giong");
    std::fs::copy(crate::harness::GIONG, &exe).unwrap();
    // The release: an archive laid out as the release workflow makes it.
    let next = "1.99.0";
    let name = format!("giong-{next}-{triple}");
    let site = dir.join("site").join(format!("v{next}"));
    std::fs::create_dir_all(site.join(&name)).unwrap();
    let new_bin = site.join(&name).join("giong");
    std::fs::write(&new_bin, format!("#!/bin/sh\necho giong {next}\n")).unwrap();
    std::fs::set_permissions(&new_bin, std::fs::Permissions::from_mode(0o755)).unwrap();
    let archive = format!("{name}.tar.gz");
    let ok = std::process::Command::new("tar")
        .args(["-czf", &archive, &name])
        .current_dir(&site)
        .status()
        .unwrap()
        .success();
    assert!(ok);
    let sum = std::process::Command::new("sha256sum")
        .arg(&archive)
        .current_dir(&site)
        .output()
        .unwrap();
    std::fs::write(site.join(format!("{archive}.sha256")), &sum.stdout).unwrap();
    let base = serve(dir.join("site"));

    let run = |args: &[&str]| {
        let out = std::process::Command::new(&exe)
            .args(args)
            .env("GIONG_INSTALL", &bin)
            .env("GIONG_DOWNLOAD_URL", &base)
            .env("NO_COLOR", "1")
            .output()
            .unwrap();
        (
            out.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&out.stdout).into_owned(),
            String::from_utf8_lossy(&out.stderr).into_owned(),
        )
    };
    // A damaged download is refused, and the command stays.
    let good = std::fs::read(site.join(format!("{archive}.sha256"))).unwrap();
    std::fs::write(
        site.join(format!("{archive}.sha256")),
        format!("{}  {archive}\n", "0".repeat(64)),
    )
    .unwrap();
    let (code, stdout, stderr) = run(&["upgrade", "--self", "--to", next, "--json"]);
    assert_eq!(code, 2, "{stdout}\n{stderr}");
    let v: Value = serde_json::from_str(stdout.trim()).unwrap();
    assert_eq!(v["error"]["code"], "GIONG-E5096", "{v:#}");
    assert_eq!(
        std::fs::read(&exe).unwrap(),
        std::fs::read(crate::harness::GIONG).unwrap()
    );
    std::fs::write(site.join(format!("{archive}.sha256")), good).unwrap();

    let (code, stdout, stderr) = run(&["upgrade", "--self", "--to", next, "--dry-run", "--json"]);
    assert_eq!(code, 0, "{stdout}\n{stderr}");
    let v: Value = serde_json::from_str(stdout.trim()).unwrap();
    assert_eq!(v["data"]["install"]["method"], "install-script", "{v:#}");

    let (code, stdout, stderr) = run(&["upgrade", "--self", "--to", next, "--json"]);
    assert_eq!(code, 0, "{stdout}\n{stderr}");
    let v: Value = serde_json::from_str(stdout.trim()).unwrap();
    assert_eq!(v["data"]["to"], next, "{v:#}");
    let (_, version, _) = run(&["--version"]);
    assert_eq!(version.trim(), format!("giong {next}"));
    assert!(!bin.join("giong.new").exists());
}
