//! Which giong release a project is on, and which one is the newest.
//!
//! The giong crates are released together with one version (docs/STABILITY.md,
//! "Versions of the crates"), so a project is on one release when every giong
//! crate in its `Cargo.lock` has the same version, and "the version to move
//! to" is one number. `giong doctor` reports a project that is behind or
//! mixed; `giong upgrade` moves it.
//!
//! The newest release comes from `cargo info giong`, which uses the person's
//! own cargo configuration (proxies, `CARGO_NET_OFFLINE`, credentials), or
//! from `GIONG_LATEST` (a mirror cargo info cannot read, a company that
//! approves releases, a test). Nothing here touches the network on its own:
//! only [`latest`] does, and only when asked.

use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};

use crate::host::{self, Version};
pub use crate::lockfile::{parse_lock, Locked, Source};
pub use crate::manifest::{allows, moved_req, parse_req};

/// The crates released together, giong-cli included. A crate added to the
/// workspace for release joins this list.
pub const CRATES: &[&str] = &[
    "giong",
    "giong-cli",
    "giong-compat",
    "giong-grpc",
    "giong-grpc-build",
    "giong-http",
    "giong-mcp",
    "giong-mysql",
    "giong-postgres",
    "giong-proto",
    "giong-quic",
    "giong-redis",
    "giong-tls",
    "giong-web",
];

pub fn is_giong_crate(name: &str) -> bool {
    CRATES.contains(&name)
}

/// The version of this command, which is a giong release.
pub fn cli() -> Version {
    host::parse_version(env!("CARGO_PKG_VERSION")).expect("our own version parses")
}

/// The giong crates of a lock that come from a registry: the ones a release
/// moves. A checkout (path) or a git branch is the person's to update.
pub fn giong_from_registry(lock: &[Locked]) -> Vec<&Locked> {
    lock.iter()
        .filter(|p| is_giong_crate(&p.name) && p.source == Source::Registry)
        .collect()
}

/// `~/.giong` (or `$GIONG_HOME`), where giong keeps what is not a project's.
pub fn giong_home() -> Option<PathBuf> {
    if let Some(home) = std::env::var_os("GIONG_HOME").filter(|h| !h.is_empty()) {
        return Some(PathBuf::from(home));
    }
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .filter(|h| !h.is_empty())?;
    Some(PathBuf::from(home).join(".giong"))
}

/// How long `doctor` trusts the newest release it last read.
pub const CACHE_FOR: Duration = Duration::from_secs(24 * 3600);

/// How long `cargo info` may take before giong gives up on it.
pub const LOOKUP_TIMEOUT: Duration = Duration::from_secs(15);

/// The newest giong release and where it was read.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Latest {
    pub version: Version,
    /// `crates.io` (through cargo), `GIONG_LATEST`, or `cache`.
    pub from: &'static str,
}

impl Latest {
    pub fn to_json(&self) -> Value {
        json!({"version": self.version.to_string(), "from": self.from})
    }
}

/// Whether cargo is told to stay offline (`CARGO_NET_OFFLINE`).
pub fn cargo_offline() -> bool {
    std::env::var("CARGO_NET_OFFLINE").is_ok_and(|v| v == "true" || v == "1")
}

/// The newest giong release. `cached` accepts an answer read within
/// [`CACHE_FOR`] (what `doctor` does; `upgrade` always asks). The error says
/// why it is not known, in words for a person.
pub fn latest(cached: bool) -> Result<Latest, String> {
    if let Ok(v) = std::env::var("GIONG_LATEST") {
        return match host::parse_version(&v) {
            Some(version) => Ok(Latest {
                version,
                from: "GIONG_LATEST",
            }),
            None => Err(format!("GIONG_LATEST={v:?} is not a version")),
        };
    }
    if cargo_offline() {
        return Err("CARGO_NET_OFFLINE is set".into());
    }
    if cached {
        if let Some(version) = read_cache() {
            return Ok(Latest {
                version,
                from: "cache",
            });
        }
    }
    let version = ask_cargo()?;
    write_cache(version);
    Ok(Latest {
        version,
        from: "crates.io",
    })
}

/// `cargo info giong`, run outside any project so that a lock file does not
/// answer instead of the registry. In `~/.giong`, not the temporary
/// directory: cargo reads `.cargo/config.toml` from every folder above the one
/// it runs in, and any user can make `/tmp/.cargo/config.toml` (a source
/// replacement there would name another latest version).
fn ask_cargo() -> Result<Version, String> {
    let dir = giong_home().ok_or("there is no home folder to run cargo info in")?;
    std::fs::create_dir_all(&dir)
        .map_err(|e| format!("{} could not be made: {e}", dir.display()))?;
    let mut cmd = crate::project::cargo();
    cmd.args(["info", "giong"])
        .current_dir(&dir)
        .env("CARGO_TERM_COLOR", "never");
    match host::run_once_within(&mut cmd, LOOKUP_TIMEOUT) {
        Ok(out) => parse_cargo_info(&out)
            .ok_or_else(|| "cargo info giong printed no version giong could read".to_string()),
        Err(host::ToolError::Missing(_)) => Err("cargo was not found".into()),
        Err(host::ToolError::TimedOut) => Err(format!(
            "cargo info giong did not answer within {} seconds",
            LOOKUP_TIMEOUT.as_secs()
        )),
        Err(host::ToolError::Failed(stderr)) => {
            let first = crate::project::first_error(&stderr);
            if first.contains("no such command") || first.contains("no such subcommand") {
                Err(
                    "this cargo has no `cargo info` (it came in Rust 1.82): rustup update stable"
                        .into(),
                )
            } else {
                Err(format!("cargo info giong failed: {first}"))
            }
        }
    }
}

/// The version `cargo info` printed: `version: 1.5.0`, or the newest one in
/// `version: 1.4.2 (latest 1.5.0)`.
pub fn parse_cargo_info(out: &str) -> Option<Version> {
    let out = crate::build::strip_ansi(out);
    let line = out
        .lines()
        .map(str::trim)
        .find_map(|l| l.strip_prefix("version:"))?;
    if let Some(after) = line.split("latest").nth(1) {
        return host::parse_version(after.trim().trim_end_matches(')'));
    }
    host::parse_version(line.trim())
}

fn cache_file() -> Option<PathBuf> {
    Some(giong_home()?.join("cache").join("releases.json"))
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn read_cache() -> Option<Version> {
    let text = std::fs::read_to_string(cache_file()?).ok()?;
    let v: Value = serde_json::from_str(&text).ok()?;
    let at = v["checked_at"].as_u64()?;
    let now = now_secs();
    if at > now || now - at > CACHE_FOR.as_secs() {
        return None;
    }
    host::parse_version(v["giong"].as_str()?)
}

/// Best effort: a home that cannot be written only costs a lookup next time.
fn write_cache(version: Version) {
    let Some(file) = cache_file() else { return };
    let Some(dir) = file.parent() else { return };
    if std::fs::create_dir_all(dir).is_err() {
        return;
    }
    let doc = json!({"giong": version.to_string(), "checked_at": now_secs()});
    let tmp = dir.join(format!("releases.json.{}", std::process::id()));
    if std::fs::write(&tmp, format!("{doc}\n")).is_ok() && std::fs::rename(&tmp, &file).is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(s: &str) -> Version {
        host::parse_version(s).unwrap()
    }

    #[test]
    fn only_giong_crates_from_a_registry_move() {
        let lock = "[[package]]\nname = \"giong\"\nversion = \"1.5.0\"\nsource = \"registry+x\"\n\n\
                    [[package]]\nname = \"giong-web\"\nversion = \"1.4.2\"\nsource = \"registry+x\"\n\n\
                    [[package]]\nname = \"giong-http\"\nversion = \"1.5.0\"\nsource = \"git+https://x#a\"\n\n\
                    [[package]]\nname = \"giong-tls\"\nversion = \"1.5.0\"\n\n\
                    [[package]]\nname = \"serde\"\nversion = \"1.0.0\"\nsource = \"registry+x\"\n";
        let all = parse_lock(lock);
        let reg: Vec<_> = giong_from_registry(&all).iter().map(|p| &p.name).collect();
        assert_eq!(reg, ["giong", "giong-web"]);
    }

    #[test]
    fn cargo_info_output_is_read() {
        let latest =
            "giong #async\nGióng — a runtime\nversion: 1.5.0\nlicense: MIT\nrust-version: 1.80\n";
        assert_eq!(parse_cargo_info(latest), Some(v("1.5.0")));
        let locked = "giong\nversion: 1.4.2 (latest 1.5.0)\nrust-version: 1.80\n";
        assert_eq!(parse_cargo_info(locked), Some(v("1.5.0")));
        let coloured = "\x1b[1mversion:\x1b[0m 1.6.1\n";
        assert_eq!(parse_cargo_info(coloured), Some(v("1.6.1")));
        assert_eq!(parse_cargo_info("error: nothing\n"), None);
    }

    #[test]
    fn every_released_crate_of_the_workspace_is_listed() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
        let Ok(manifest) = std::fs::read_to_string(root.join("Cargo.toml")) else {
            return; // the published crate has no workspace
        };
        let Some(members) = manifest.split("members = [").nth(1) else {
            return;
        };
        let members = members.split(']').next().unwrap_or("");
        for m in members.split(',') {
            let m = m.trim().trim_matches('"');
            if m.is_empty() || m.starts_with("examples") || m.starts_with("bench") {
                continue;
            }
            let Ok(text) = std::fs::read_to_string(root.join(m).join("Cargo.toml")) else {
                continue;
            };
            if text.contains("publish = false") {
                continue;
            }
            let name = text
                .lines()
                .find_map(|l| l.strip_prefix("name = "))
                .map(|n| n.trim_matches('"').to_string())
                .unwrap();
            assert!(
                is_giong_crate(&name),
                "{name} is released: add it to CRATES"
            );
        }
        assert!(is_giong_crate("giong"));
    }
}
