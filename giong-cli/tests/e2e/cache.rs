//! `giong cache` on real cargo builds: two projects with the same dependency
//! (from a git repository on disk, so no network) end up with one copy of its
//! build, and cargo sees both builds as fresh afterwards.

use std::path::Path;
use std::process::Command;

use serde_json::Value;

use crate::harness::{giong, toolchain, TempDir};

fn cargo(home: &Path) -> Command {
    let mut c = Command::new(std::env::var_os("CARGO").unwrap_or("cargo".into()));
    c.env_remove("CARGO_TARGET_DIR")
        .env_remove("CARGO_BUILD_BUILD_DIR")
        .env_remove("RUSTFLAGS")
        .env("CARGO_HOME", home)
        .env("CARGO_TERM_COLOR", "never");
    c
}

/// The store needs cargo's build-dir layout (cargo 1.100).
fn new_cargo() -> bool {
    let out = Command::new(std::env::var_os("CARGO").unwrap_or("cargo".into()))
        .arg("--version")
        .output();
    let minor = out
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .and_then(|t| {
            t.split_whitespace()
                .nth(1)?
                .split('.')
                .nth(1)?
                .parse::<u32>()
                .ok()
        });
    minor.is_some_and(|m| m >= 100)
}

fn git(dir: &Path, args: &[&str]) {
    let ok = Command::new("git")
        .args([
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@example.com",
            "-c",
            "commit.gpgsign=false",
        ])
        .args(args)
        .current_dir(dir)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    assert!(ok, "git {args:?}");
}

/// Units cargo did not find fresh in `cargo build --message-format=json`.
fn rebuilt(dir: &Path, home: &Path) -> Vec<String> {
    let out = cargo(home)
        .args(["build", "--message-format=json"])
        .current_dir(dir)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .filter(|v| v["reason"] == "compiler-artifact" && v["fresh"] == false)
        .map(|v| v["target"]["name"].as_str().unwrap_or("").to_string())
        .collect()
}

#[test]
fn two_projects_share_one_build_of_their_dependency() {
    if !toolchain("two_projects_share_one_build_of_their_dependency")
        || !new_cargo()
        || Command::new("git").arg("--version").output().is_err()
    {
        return;
    }
    let t = TempDir::new("cache-share");
    let home = t.join("cargo-home");
    // A dependency big enough to be shared (16 KiB and more).
    let mut lib = String::new();
    for i in 0..300 {
        lib.push_str(&format!(
            "pub fn f{i}(x: u64) -> String {{ format!(\"{i}: {{}}\", x.wrapping_mul({i})) }}\n"
        ));
    }
    t.write(
        "dep/Cargo.toml",
        "[package]\nname = \"shared-dep\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    );
    t.write("dep/src/lib.rs", &lib);
    let dep = t.join("dep");
    git(&dep, &["init", "-q"]);
    git(&dep, &["add", "."]);
    git(&dep, &["commit", "-q", "-m", "dep"]);
    let url = format!(
        "file:///{}",
        dep.display()
            .to_string()
            .replace('\\', "/")
            .trim_start_matches('/')
    );
    for name in ["one", "two"] {
        t.write(
            &format!("{name}/Cargo.toml"),
            &format!(
                "[package]\nname = \"{name}\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n\
                 [dependencies]\nshared-dep = {{ git = \"{url}\" }}\n\n[workspace]\n"
            ),
        );
        t.write(
            &format!("{name}/src/main.rs"),
            "fn main() { println!(\"{}\", shared_dep::f7(6)); }\n",
        );
        let out = cargo(&home)
            .args(["build", "--quiet"])
            .current_dir(t.join(name))
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    let store = t.join("store");
    let out = giong(["cache", "share", "--json"])
        .arg(t.path())
        .env("GIONG_STORE_DIR", &store)
        .run();
    let r = out.report("cache");
    // The rlib, and the rmeta when it is as big.
    assert!(
        r["data"]["linked_files"].as_u64() >= Some(1),
        "{}",
        out.dump()
    );
    assert!(r["data"]["bytes_after"].as_u64() < r["data"]["bytes_before"].as_u64());

    // Cargo rebuilds nothing, in either project, and the program still runs.
    for name in ["one", "two"] {
        assert_eq!(
            rebuilt(&t.join(name), &home),
            Vec::<String>::new(),
            "{name}"
        );
    }
    let run = cargo(&home)
        .args(["run", "--quiet"])
        .current_dir(t.join("two"))
        .output()
        .unwrap();
    assert_eq!(String::from_utf8_lossy(&run.stdout).trim(), "7: 42");

    // A change to the project's own code rebuilds only that.
    t.write(
        "two/src/main.rs",
        "fn main() { println!(\"{}\", shared_dep::f8(6)); }\n",
    );
    assert_eq!(rebuilt(&t.join("two"), &home), vec!["two".to_string()]);
    assert_eq!(rebuilt(&t.join("one"), &home), Vec::<String>::new());

    let status = giong(["cache", "--json"])
        .env("GIONG_STORE_DIR", &store)
        .run();
    let s = status.report("cache");
    assert!(
        s["data"]["saved_bytes"].as_u64().unwrap() >= 16 * 1024,
        "{}",
        status.dump()
    );
    assert_eq!(s["data"]["projects"].as_array().map(Vec::len), Some(2));

    // Nothing to remove while both projects link the build.
    let gc = giong(["cache", "gc", "--budget", "off", "--json"])
        .env("GIONG_STORE_DIR", &store)
        .run();
    assert_eq!(
        gc.report("cache")["data"]["unused_objects"],
        0,
        "{}",
        gc.dump()
    );
}
