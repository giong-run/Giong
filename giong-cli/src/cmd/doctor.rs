//! `giong doctor`: is this machine and this project ready for giong?
//!
//! The checks are the facts of docs/DEPLOYMENT.md turned into code: the Rust
//! version, a linker, the kernel and io_uring, the locked-memory and
//! open-files limits, a container's seccomp profile; and in a project, crates
//! that need Tokio, the dev profile, `.env` against `.env.example`, a port that
//! is taken, and the disk the builds use.

use std::net::TcpListener;
use std::path::Path;

use serde_json::json;

use super::Command;
use crate::args::{Args, Flag, Kind};
use crate::code::Code;
use crate::dotenv;
use crate::host::{self, Version};
use crate::project::Project;
use crate::report::{Edit, Failure, Finding, Report, Severity};

pub const COMMAND: Command = Command {
    name: "doctor",
    summary: "check that this machine and project are ready for giong",
    usage: "giong doctor [--machine]\n\n\
            Checks the Rust toolchain, the linker, the kernel and io_uring, the locked-memory and \
            open-files limits and a container's seccomp profile; inside a project also crates that \
            need Tokio, the dev profile, .env against .env.example, the port the service uses and \
            the disk its builds take. Every problem comes with its code and its fix. Exit status 1 \
            when one is an error.",
    flags: &[Flag {
        name: "machine",
        kind: Kind::Switch,
        help: "check only the machine, not the project in this directory",
    }],
    max_words: 0,
    run,
};

/// The oldest Rust the giong crates build with.
pub const MSRV: Version = Version(1, 80, 0);
/// giong-quic, and HTTP/3 in giong-http.
pub const MSRV_QUIC: Version = Version(1, 85, 0);

/// Crates that need the Tokio runtime, with the giong crate that replaces them.
pub const TOKIO_CRATES: &[(&str, &str)] = &[
    ("tokio", "giong (runtime, net, fs, time, sync)"),
    ("sqlx", "giong-postgres or giong-mysql"),
    ("tokio-postgres", "giong-postgres"),
    ("deadpool-postgres", "giong-postgres's Pool"),
    (
        "bb8",
        "the per-worker Pool of giong-postgres, giong-mysql or giong-redis",
    ),
    ("mysql_async", "giong-mysql"),
    ("reqwest", "giong-http's client (feature `client`)"),
    ("hyper-util", "giong-http"),
    ("axum", "giong-web"),
    ("warp", "giong-web"),
    ("actix-web", "giong-web"),
    ("poem", "giong-web"),
    ("salvo", "giong-web"),
    ("rocket", "giong-web"),
    ("tonic", "giong-grpc"),
    ("tokio-rustls", "giong-tls"),
    ("quinn", "giong-quic"),
    ("async-std", "giong"),
    ("smol", "giong"),
];

fn run(args: &Args) -> Result<Report, Failure> {
    let mut r = Report::new("doctor");
    machine(&mut r);
    if !args.has("machine") {
        if let Ok(project) = Project::here() {
            project_checks(&project, &mut r);
        } else {
            r.data("project", json!(null));
        }
    }
    let errors = r
        .findings()
        .iter()
        .filter(|f| f.severity == Severity::Error)
        .count();
    let warnings = r
        .findings()
        .iter()
        .filter(|f| f.severity == Severity::Warning)
        .count();
    r.line(if errors + warnings == 0 {
        "Everything giong needs is here.".to_string()
    } else {
        format!(
            "{errors} {}, {warnings} {}:",
            if errors == 1 { "error" } else { "errors" },
            if warnings == 1 { "warning" } else { "warnings" }
        )
    });
    Ok(r)
}

fn machine(r: &mut Report) {
    r.data("os", json!(std::env::consts::OS));
    r.data("arch", json!(std::env::consts::ARCH));
    r.data("cpus", json!(host::cpus()));

    match host::rustc_version() {
        None => r.push(
            Finding::new(Code::CARGO_FAILED, Severity::Error, "rustc was not found")
                .detail("giong needs the Rust toolchain on the PATH")
                .fix("install Rust with rustup (https://rustup.rs) and open a new terminal"),
        ),
        Some(v) => {
            r.data("rustc", json!(v.to_string()));
            if v < MSRV {
                r.push(rust_too_old(v, MSRV, "the giong crates"));
            }
        }
    }

    if cfg!(unix) && !host::exists("cc", &["--version"]) {
        r.push(
            Finding::new(Code::NO_LINKER, Severity::Error, "no C linker (cc) found")
                .detail("Rust links binaries with the system's cc")
                .fix(if cfg!(target_os = "macos") {
                    "xcode-select --install"
                } else {
                    "install build-essential (Debian, Ubuntu) or gcc (Fedora, Arch)"
                }),
        );
    }

    let driver = if cfg!(target_os = "linux") {
        linux(r)
    } else if cfg!(target_os = "macos") {
        macos(r);
        "kqueue"
    } else if cfg!(windows) {
        r.line(
            "Note: on Windows, giong 1.4 runs the giong crate only; giong-http, giong-web, the \
             database crates and giong-mcp need Linux or macOS. WSL 2 runs all of them.",
        );
        r.data("windows_runtime_only", json!(true));
        "iocp"
    } else {
        "unsupported"
    };
    r.data("driver", json!(driver));
}

/// The Linux checks; returns the driver a worker would most likely get.
fn linux(r: &mut Report) -> &'static str {
    let kernel = host::kernel_version();
    r.data("kernel", json!(kernel.map(|k| k.to_string())));
    let mut uring = true;
    if let Some(k) = kernel {
        if k < Version(6, 0, 0) {
            uring = false;
            r.push(
                Finding::new(
                    Code::runtime(2, "the kernel is too old"),
                    Severity::Warning,
                    format!("Linux {k} has no io_uring giong can use: workers run on epoll"),
                )
                .detail("giong's io_uring driver needs Linux 6.0 or newer")
                .fix("run a 6.x kernel for io_uring (epoll works, about 10% slower)"),
            );
        }
    }
    match host::io_uring_disabled() {
        Some(2) => {
            uring = false;
            r.push(
                Finding::new(
                    Code::runtime(1, "io_uring is not available"),
                    Severity::Warning,
                    "io_uring is turned off on this machine (kernel.io_uring_disabled = 2)",
                )
                .detail("giong falls back to its epoll driver")
                .fix("sysctl -w kernel.io_uring_disabled=0 (as root) to allow it"),
            );
        }
        Some(1) => r.push(
            Finding::new(
                Code::runtime(1, "io_uring is not available"),
                Severity::Info,
                "io_uring is limited to privileged processes and one group (kernel.io_uring_disabled = 1)",
            )
            .detail("an unprivileged service outside kernel.io_uring_group runs on epoll")
            .fix("put the service's user in the group named by kernel.io_uring_group, or set the sysctl to 0"),
        ),
        _ => {}
    }
    if host::in_container() && host::seccomp() == Some(2) {
        uring = false;
        r.push(
            Finding::new(
                Code::DOCKER_SECCOMP,
                Severity::Warning,
                "this container's seccomp filter probably blocks io_uring",
            )
            .detail("Docker's and Kubernetes' default profiles do; giong then uses epoll")
            .fix("run with giong-seccomp.json (docs/DEPLOYMENT.md, Docker), or accept epoll for development"),
        );
    }
    if let Some(k) = kernel {
        if uring && counts_rings_as_locked(k) && !host::is_root_outside_container() {
            let need = 150 * 1024 * host::cpus() as u64;
            if let Some(l) = host::limit("Max locked memory") {
                if let Some(soft) = l.soft.filter(|&s| s < need) {
                    r.push(
                        Finding::new(
                            Code::runtime(10, "the locked-memory limit is used up"),
                            Severity::Warning,
                            format!(
                                "the locked-memory limit ({} KiB) holds {} of {} workers",
                                soft / 1024,
                                soft / (150 * 1024),
                                host::cpus()
                            ),
                        )
                        .detail(format!(
                            "Linux {k} counts each io_uring ring (about 150 KiB) against ulimit -l; \
                             workers past the limit run on epoll (GIONG-E0010)"
                        ))
                        .fix("raise it: LimitMEMLOCK=infinity (systemd), --ulimit memlock=-1:-1 (Docker), or ulimit -l unlimited"),
                    );
                }
            }
        }
    }
    if let Some(l) = host::limit("Max open files") {
        r.data("open_files_limit", json!({"soft": l.soft, "hard": l.hard}));
        // The runtime raises the soft limit to the hard one; the hard one counts.
        if let Some(hard) = l.hard.filter(|&h| h < 65_536) {
            r.push(
                Finding::new(
                    Code::OPEN_FILES_LIMIT,
                    Severity::Warning,
                    format!("the open-files limit is {hard}: at most about {hard} connections"),
                )
                .detail("each connection, file and pipe takes one descriptor; giong raises the soft limit to the hard one")
                .fix("raise the hard limit: LimitNOFILE=1048576 (systemd), --ulimit nofile=1048576 (Docker)"),
            );
        }
    }
    if uring {
        "io_uring"
    } else {
        "epoll"
    }
}

/// Linux 6.14 to 7.2 count rings against `RLIMIT_MEMLOCK`; 6.18 stopped at 6.18.58.
pub fn counts_rings_as_locked(k: Version) -> bool {
    k >= Version(6, 14, 0) && k < Version(7, 3, 0) && !(k.0 == 6 && k.1 == 18 && k.2 >= 58)
}

fn macos(r: &mut Report) {
    if let Some(soft) = host::output("sh", &["-c", "ulimit -n"])
        .and_then(|s| s.trim().parse::<u64>().ok())
        .filter(|&n| n < 10_240)
    {
        r.push(
            Finding::new(
                Code::OPEN_FILES_LIMIT,
                Severity::Info,
                format!("the open-files limit of this shell is {soft}"),
            )
            .detail("on macOS giong does not raise it; a load test past it fails with EMFILE")
            .fix("ulimit -n 10240 in the terminal that runs the service"),
        );
    }
}

fn rust_too_old(v: Version, need: Version, who: &str) -> Finding {
    Finding::new(
        Code::RUST_TOO_OLD,
        Severity::Error,
        format!("Rust {v} is older than {need}, which {who} need"),
    )
    .detail(format!("rustc on the PATH is {v}"))
    .fix("rustup update stable")
}

fn project_checks(p: &Project, r: &mut Report) {
    r.data("project", json!(p.root.display().to_string()));
    // (name, features, manifest relative to the root)
    let mut deps: Vec<(String, Vec<String>, String)> = Vec::new();
    for pkg in p.packages() {
        let manifest = pkg["manifest_path"]
            .as_str()
            .map(|m| {
                Path::new(m)
                    .strip_prefix(&p.root)
                    .map(|r| r.display().to_string())
                    .unwrap_or_else(|_| m.to_string())
            })
            .unwrap_or_else(|| "Cargo.toml".into());
        for d in pkg["dependencies"].as_array().into_iter().flatten() {
            if d["kind"].is_null() || d["kind"] == "normal" {
                let name = d["name"].as_str().unwrap_or("").to_string();
                let features = d["features"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|f| f.as_str().map(String::from))
                    .collect();
                deps.push((name, features, manifest.clone()));
            }
        }
    }
    for (name, replacement) in TOKIO_CRATES {
        let mut seen = std::collections::HashSet::new();
        for (d, features, manifest) in &deps {
            if d != name || !needs_tokio_runtime(d, features) || !seen.insert(manifest) {
                continue;
            }
            let text = std::fs::read_to_string(p.root.join(manifest)).unwrap_or_default();
            let (line, col) = find_line(&text, name);
            r.push(
                Finding::new(
                    Code::TOKIO_CRATE,
                    Severity::Warning,
                    format!("{name} needs the Tokio runtime"),
                )
                .detail(format!(
                    "on a giong worker it panics with \"there is no reactor running\"; the giong \
                     crate for the job is {replacement}"
                ))
                .fix(format!("replace {name} with {replacement}"))
                .at(manifest.clone(), line, col),
            );
        }
    }
    let quic = deps.iter().any(|(d, f, _)| {
        d == "giong-quic" || (d == "giong-http" && f.iter().any(|f| f == "unstable-http3"))
    });
    if quic {
        if let Some(v) = host::rustc_version().filter(|v| *v < MSRV_QUIC) {
            r.push(rust_too_old(v, MSRV_QUIC, "giong-quic and HTTP/3"));
        }
    }
    if let Some(f) = dev_profile_finding(&p.manifest()) {
        r.push(f);
    }
    env_checks(&p.root, r);
    let bytes = dir_size(&p.target_dir);
    r.data("target_dir", json!(p.target_dir.display().to_string()));
    r.data("target_bytes", json!(bytes));
    if bytes > 10 * (1 << 30) {
        r.push(
            Finding::new(
                Code::DISK_USED,
                Severity::Warning,
                format!("{} holds {:.1} GiB of builds", p.target_dir.display(), bytes as f64 / (1u64 << 30) as f64),
            )
            .detail("cargo never removes old builds; each toolchain update and changed dependency leaves the old ones")
            .fix("giong clean"),
        );
    }
}

/// Tokio without runtime features (its traits and `sync`, as giong-compat
/// uses) runs anywhere; every other crate in the list needs the runtime.
pub fn needs_tokio_runtime(name: &str, features: &[String]) -> bool {
    if name != "tokio" {
        return true;
    }
    const RUNTIME: &[&str] = &[
        "full",
        "rt",
        "rt-multi-thread",
        "macros",
        "net",
        "time",
        "fs",
        "process",
        "signal",
        "io-std",
    ];
    features.iter().any(|f| RUNTIME.contains(&f.as_str()))
}

/// The dev profile's `debug` setting, with an edit to `line-tables-only` when
/// it writes full debug information.
pub fn dev_profile_finding(manifest: &str) -> Option<Finding> {
    let finding = || {
        Finding::new(
            Code::SLOW_DEBUG_INFO,
            Severity::Info,
            "debug builds write full debug information",
        )
        .detail("line tables are enough for backtraces, and make rebuilds faster and target/ smaller (measured: 181 to 140 MB for a small giong-web service)")
        .fix("set debug = \"line-tables-only\" under [profile.dev] in Cargo.toml")
    };
    let mut offset = 0;
    let mut header_end = None;
    for line in manifest.split_inclusive('\n') {
        let t = line.trim();
        if header_end.is_none() {
            if t == "[profile.dev]" {
                header_end = Some(offset + line.len());
            }
        } else if t.starts_with('[') {
            break;
        } else if let Some(rest) = t.strip_prefix("debug") {
            let rest = rest.trim_start();
            if let Some(value) = rest.strip_prefix('=') {
                let value = value.split('#').next().unwrap_or("").trim();
                let full = matches!(value, "true" | "2" | "\"full\"");
                if !full {
                    return None;
                }
                let start = offset + line.find(value).unwrap_or(0);
                let line_no = manifest[..offset].matches('\n').count() as u32 + 1;
                return Some(finding().at("Cargo.toml", line_no, 1).edits(vec![Edit {
                    file: "Cargo.toml".into(),
                    start,
                    end: start + value.len(),
                    replacement: "\"line-tables-only\"".into(),
                }]));
            }
        }
        offset += line.len();
    }
    let (start, replacement) = match header_end {
        Some(at) => (at, "debug = \"line-tables-only\"\n".to_string()),
        None => {
            let sep = if manifest.is_empty() || manifest.ends_with("\n\n") {
                ""
            } else if manifest.ends_with('\n') {
                "\n"
            } else {
                "\n\n"
            };
            (
                manifest.len(),
                format!("{sep}[profile.dev]\ndebug = \"line-tables-only\"\n"),
            )
        }
    };
    Some(finding().edits(vec![Edit {
        file: "Cargo.toml".into(),
        start,
        end: start,
        replacement,
    }]))
}

fn env_checks(root: &Path, r: &mut Report) {
    let Some((example, _)) = dotenv::read(&root.join(".env.example")) else {
        return;
    };
    let local = dotenv::read(&root.join(".env"))
        .map(|(v, _)| v)
        .unwrap_or_default();
    let mut values = std::collections::HashMap::new();
    for v in &local {
        values.insert(v.key.clone(), v.value.clone());
    }
    for v in &example {
        let set = values.contains_key(&v.key) || std::env::var_os(&v.key).is_some();
        if !set {
            r.push(
                Finding::new(
                    Code::ENV_MISSING,
                    Severity::Warning,
                    format!("{} is in .env.example but not set", v.key),
                )
                .detail("neither .env nor the environment sets it")
                .fix(format!(
                    "copy the line for {} from .env.example into .env and fill it in",
                    v.key
                ))
                .at(".env.example", v.line, 1),
            );
        }
    }
    let listen = ["LISTEN", "ADDR", "BIND"]
        .iter()
        .find_map(|k| values.get(*k).cloned())
        .or_else(|| values.get("PORT").map(|p| format!("0.0.0.0:{p}")));
    if let Some(addr) = listen {
        if let Ok(sock) = addr.parse::<std::net::SocketAddr>() {
            if let Err(e) = TcpListener::bind(sock) {
                if e.kind() == std::io::ErrorKind::AddrInUse {
                    r.push(
                        Finding::new(
                            Code::PORT_IN_USE,
                            Severity::Warning,
                            format!("{addr} is already in use"),
                        )
                        .detail("the service is configured to listen there (.env), and another process does")
                        .fix(if cfg!(target_os = "macos") {
                            format!("lsof -i :{} shows which process; stop it or change the port in .env", sock.port())
                        } else {
                            format!("ss -ltnp 'sport = :{}' shows which process; stop it or change the port in .env", sock.port())
                        }),
                    );
                }
            }
        }
    }
}

/// The line and column where `word` first appears as a key (`word =` or `word.`).
fn find_line(text: &str, word: &str) -> (u32, u32) {
    for (i, line) in text.lines().enumerate() {
        let t = line.trim_start();
        if let Some(rest) = t.strip_prefix(word) {
            let rest = rest.trim_start();
            if rest.starts_with('=') || rest.starts_with('.') {
                return (i as u32 + 1, (line.len() - t.len()) as u32 + 1);
            }
        }
    }
    (1, 1)
}

/// Bytes in files under `dir` (not following symbolic links).
pub fn dir_size(dir: &Path) -> u64 {
    let mut total = 0;
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&d) else {
            continue;
        };
        for e in entries.flatten() {
            let Ok(meta) = e.path().symlink_metadata() else {
                continue;
            };
            if meta.is_dir() {
                stack.push(e.path());
            } else if meta.is_file() {
                total += meta.len();
            }
        }
    }
    total
}

#[cfg(test)]
mod tests {
    use super::*;

    fn apply(text: &str, f: &Finding) -> String {
        let mut s = text.to_string();
        for e in f.edits.iter().rev() {
            s.replace_range(e.start..e.end, &e.replacement);
        }
        s
    }

    #[test]
    fn dev_profile_is_fixed_in_every_shape() {
        // No profile at all: one is appended.
        let m = "[package]\nname = \"a\"\n";
        let f = dev_profile_finding(m).unwrap();
        assert!(f.fixable);
        assert_eq!(
            apply(m, &f),
            "[package]\nname = \"a\"\n\n[profile.dev]\ndebug = \"line-tables-only\"\n"
        );
        // A profile without `debug`: the key is added under the header.
        let m = "[profile.dev]\nopt-level = 1\n\n[dependencies]\n";
        let f = dev_profile_finding(m).unwrap();
        assert_eq!(
            apply(m, &f),
            "[profile.dev]\ndebug = \"line-tables-only\"\nopt-level = 1\n\n[dependencies]\n"
        );
        // Full debug info written out: the value is replaced.
        for v in ["true", "2", "\"full\""] {
            let m = format!("[profile.dev]\ndebug = {v} # why\n");
            let f = dev_profile_finding(&m).unwrap();
            assert_eq!(
                apply(&m, &f),
                "[profile.dev]\ndebug = \"line-tables-only\" # why\n"
            );
            assert_eq!(f.place.as_ref().unwrap().line, 2);
        }
        // Already light: nothing to say.
        for v in ["\"line-tables-only\"", "0", "false", "1", "\"limited\""] {
            assert!(dev_profile_finding(&format!("[profile.dev]\ndebug = {v}\n")).is_none());
        }
        // `debug` of another profile does not count.
        let m = "[profile.release]\ndebug = true\n";
        assert!(dev_profile_finding(m).is_some());
        assert!(apply(m, &dev_profile_finding(m).unwrap())
            .ends_with("[profile.dev]\ndebug = \"line-tables-only\"\n"));
    }

    #[test]
    fn tokio_only_counts_with_its_runtime() {
        let f = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert!(!needs_tokio_runtime("tokio", &f(&[])));
        assert!(!needs_tokio_runtime("tokio", &f(&["io-util", "sync"])));
        assert!(needs_tokio_runtime("tokio", &f(&["full"])));
        assert!(needs_tokio_runtime("tokio", &f(&["sync", "rt"])));
        assert!(needs_tokio_runtime("sqlx", &f(&[])));
    }

    #[test]
    fn which_kernels_count_rings() {
        assert!(!counts_rings_as_locked(Version(6, 13, 9)));
        assert!(counts_rings_as_locked(Version(6, 14, 0)));
        assert!(counts_rings_as_locked(Version(6, 18, 44)));
        assert!(!counts_rings_as_locked(Version(6, 18, 58)));
        assert!(counts_rings_as_locked(Version(7, 2, 5)));
        assert!(!counts_rings_as_locked(Version(7, 3, 0)));
    }

    #[test]
    fn manifest_lines() {
        let m = "[dependencies]\nserde = \"1\"\n  sqlx = { version = \"0.8\" }\ntokio.workspace = true\n";
        assert_eq!(find_line(m, "sqlx"), (3, 3));
        assert_eq!(find_line(m, "tokio"), (4, 1));
        assert_eq!(find_line(m, "axum"), (1, 1));
    }

    #[test]
    fn dir_size_counts_files() {
        let d = std::env::temp_dir().join(format!("giong-cli-size-{}", std::process::id()));
        std::fs::create_dir_all(d.join("a/b")).unwrap();
        std::fs::write(d.join("x"), [0u8; 10]).unwrap();
        std::fs::write(d.join("a/b/y"), [0u8; 5]).unwrap();
        assert_eq!(dir_size(&d), 15);
        std::fs::remove_dir_all(&d).unwrap();
        assert_eq!(dir_size(&d), 0);
    }
}
