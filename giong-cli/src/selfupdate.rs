//! `giong upgrade --self`: replaces this command with another release, the
//! way it was installed, and no other way.
//!
//! - From the install script (`~/.giong/bin`, or `$GIONG_INSTALL`): the same
//!   archive the script downloads, from the same place (`$GIONG_DOWNLOAD_URL`
//!   or the GitHub release), checked against its `.sha256`, unpacked with
//!   `tar` and renamed over the old binary. Where no binary is published for
//!   this machine, `cargo install --root`, as the script does.
//! - From `cargo install` (`$CARGO_HOME/bin`): `cargo install giong-cli
//!   --locked --version <v>`.
//! - From anything else (a package manager, a copy): refused with the
//!   commands to use instead, since giong cannot know what it would break.
//!
//! On Windows a running program cannot be replaced, but it can be renamed:
//! the old `giong.exe` becomes `giong.old.exe` (removed by the next upgrade)
//! and the new one takes its name.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use serde_json::{json, Value};

use crate::code::Code;
use crate::host::{self, Version};
use crate::report::Failure;

/// Where releases are downloaded from, unless `GIONG_DOWNLOAD_URL` names a mirror.
pub const RELEASES: &str = "https://github.com/giong-run/Giong/releases/download";

/// How this command was installed.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Method {
    /// By the install script, into `dir`.
    Script { dir: PathBuf },
    /// By `cargo install`, into `root/bin`.
    Cargo { root: PathBuf },
    /// By something giong does not drive; the binary's folder.
    Unknown { dir: PathBuf },
}

impl Method {
    pub fn name(&self) -> &'static str {
        match self {
            Method::Script { .. } => "install-script",
            Method::Cargo { .. } => "cargo-install",
            Method::Unknown { .. } => "unknown",
        }
    }

    pub fn to_json(&self) -> Value {
        let dir = match self {
            Method::Script { dir } | Method::Unknown { dir } => dir.clone(),
            Method::Cargo { root } => root.join("bin"),
        };
        json!({"method": self.name(), "dir": dir.display().to_string()})
    }

    /// What it will run, in words, for `--dry-run`.
    pub fn plan(&self, target: Version) -> String {
        match self {
            Method::Script { dir } => format!(
                "download giong {target} for {} from {}, check its SHA-256 and put it in {}",
                triple().unwrap_or("this machine (built with cargo install)"),
                download_base(),
                dir.display()
            ),
            Method::Cargo { .. } => format!("cargo install giong-cli --locked --version {target}"),
            Method::Unknown { dir } => format!(
                "nothing: the command in {} was not installed by the install script or cargo",
                dir.display()
            ),
        }
    }
}

fn home() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .filter(|h| !h.is_empty())
        .map(PathBuf::from)
}

fn same_dir(a: &Path, b: &Path) -> bool {
    match (fs::canonicalize(a), fs::canonicalize(b)) {
        (Ok(a), Ok(b)) => a == b,
        _ => a == b,
    }
}

/// How the binary at `exe` was installed, from the folder it is in.
pub fn detect(exe: &Path) -> Method {
    let dir = exe.parent().unwrap_or(Path::new(".")).to_path_buf();
    let script = std::env::var_os("GIONG_INSTALL")
        .filter(|d| !d.is_empty())
        .map(PathBuf::from)
        .or_else(|| home().map(|h| h.join(".giong").join("bin")));
    if script.as_deref().is_some_and(|s| same_dir(s, &dir)) {
        return Method::Script { dir };
    }
    let mut cargo_roots: Vec<PathBuf> = Vec::new();
    for var in ["CARGO_INSTALL_ROOT", "CARGO_HOME"] {
        if let Some(r) = std::env::var_os(var).filter(|r| !r.is_empty()) {
            cargo_roots.push(PathBuf::from(r));
        }
    }
    if let Some(h) = home() {
        cargo_roots.push(h.join(".cargo"));
    }
    for root in cargo_roots {
        if same_dir(&root.join("bin"), &dir) {
            return Method::Cargo { root };
        }
    }
    Method::Unknown { dir }
}

/// The target of the published binaries for this machine, as the install
/// scripts name it.
pub fn triple() -> Option<&'static str> {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("linux", "x86_64") => Some("x86_64-unknown-linux-musl"),
        ("linux", "aarch64") => Some("aarch64-unknown-linux-musl"),
        ("macos", "x86_64") => Some("x86_64-apple-darwin"),
        ("macos", "aarch64") => Some("aarch64-apple-darwin"),
        ("windows", "x86_64") => Some("x86_64-pc-windows-msvc"),
        _ => None,
    }
}

fn download_base() -> String {
    std::env::var("GIONG_DOWNLOAD_URL")
        .ok()
        .filter(|u| !u.is_empty())
        .unwrap_or_else(|| RELEASES.to_string())
        .trim_end_matches('/')
        .to_string()
}

fn failed(what: impl Into<String>, fix: impl Into<String>) -> Failure {
    Failure::new(Code::SELF_UPDATE, what, fix)
}

/// The commands that update giong by hand, for a refusal.
pub fn by_hand(target: Version) -> String {
    format!(
        "update it the way it was installed: curl -fsSL https://giong.run/install.sh | \
         GIONG_VERSION={target} sh (Windows: install.ps1 with $env:GIONG_VERSION), or cargo \
         install giong-cli --locked --version {target}"
    )
}

/// Replaces the command at `exe` with release `target`. Progress goes to
/// standard error, so standard output stays one JSON document.
pub fn install(exe: &Path, method: &Method, target: Version) -> Result<(), Failure> {
    match method {
        Method::Unknown { dir } => Err(failed(
            format!(
                "giong cannot tell how the command in {} was installed, so it does not replace it",
                dir.display()
            ),
            by_hand(target),
        )),
        Method::Cargo { root } => cargo_install(exe, target, Some(root)),
        Method::Script { dir } => match triple() {
            Some(t) => match download(exe, target, t) {
                Ok(()) => Ok(()),
                Err(Download::NotPublished(why)) => {
                    eprintln!("giong: {why}: building it with cargo install");
                    cargo_install(exe, target, dir.parent())
                }
                Err(Download::Failed(f)) => Err(f),
            },
            None => cargo_install(exe, target, dir.parent()),
        },
    }
}

enum Download {
    /// The archive is not there (an older release, a target not built).
    NotPublished(String),
    Failed(Failure),
}

fn curl(url: &str, out: &Path) -> Result<(), String> {
    let mut cmd = Command::new("curl");
    cmd.args(["-fsSL", "--retry", "3", "--max-time", "600"]);
    cmd.args(https_only(url));
    let status = cmd
        .arg("-o")
        .arg(out)
        .arg(url)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output();
    match status {
        Ok(o) if o.status.success() => Ok(()),
        Ok(o) => Err(format!(
            "curl could not download {url}: {}",
            String::from_utf8_lossy(&o.stderr).trim()
        )),
        Err(e) => Err(format!("curl could not be started: {e}")),
    }
}

/// The curl options that keep an https download on https: a redirect to plain
/// http, which anyone on the network path could answer with another archive
/// and its matching `.sha256`, is refused, and TLS is 1.2 or later. A mirror
/// named with `http://` (a local one, as the tests use) is left as it is.
fn https_only(url: &str) -> &'static [&'static str] {
    if url.starts_with("https://") {
        &["--proto", "=https", "--proto-redir", "=https", "--tlsv1.2"]
    } else {
        &[]
    }
}

fn download(exe: &Path, target: Version, triple: &str) -> Result<(), Download> {
    let name = format!("giong-{target}-{triple}");
    let ext = if cfg!(windows) { "zip" } else { "tar.gz" };
    let base = format!("{}/v{target}", download_base());
    // A folder no other user can enter: the archive is checked, then unpacked
    // and the binary copied from it, and nothing may swap them in between.
    let work = crate::tmp::fresh("giong-upgrade").map_err(|e| {
        Download::Failed(failed(
            format!("a temporary folder could not be made: {e}"),
            "check the temporary folder (TMPDIR) is writable",
        ))
    })?;
    let result = (|| {
        let archive = work.join(format!("{name}.{ext}"));
        eprintln!("giong: downloading {base}/{name}.{ext}");
        curl(&format!("{base}/{name}.{ext}"), &archive).map_err(Download::NotPublished)?;
        let sum = work.join("sum");
        curl(&format!("{base}/{name}.{ext}.sha256"), &sum).map_err(|e| {
            Download::Failed(failed(
                e,
                "check the network, or set GIONG_DOWNLOAD_URL to a mirror of the release downloads",
            ))
        })?;
        let want = fs::read_to_string(&sum)
            .unwrap_or_default()
            .split_whitespace()
            .next()
            .unwrap_or("")
            .to_lowercase();
        let bytes = fs::read(&archive).map_err(|e| {
            Download::Failed(failed(
                format!("the download cannot be read: {e}"),
                "run it again",
            ))
        })?;
        let got = crate::sha256::hex(&bytes);
        if want != got {
            return Err(Download::Failed(failed(
                format!("the download of {name} is damaged (SHA-256 {got}, expected {want})"),
                "run giong upgrade --self again; if it repeats, the mirror serves another file",
            )));
        }
        let unpacked = Command::new("tar")
            .arg("-xf")
            .arg(&archive)
            .arg("-C")
            .arg(&work)
            .stdin(Stdio::null())
            .output();
        match unpacked {
            Ok(o) if o.status.success() => {}
            Ok(o) => {
                return Err(Download::Failed(failed(
                    format!(
                        "tar could not unpack {name}.{ext}: {}",
                        String::from_utf8_lossy(&o.stderr).trim()
                    ),
                    "run giong upgrade --self again",
                )))
            }
            Err(e) => {
                return Err(Download::Failed(failed(
                    format!("tar could not be started: {e}"),
                    "install tar (Windows 10 and later have it), or update by hand",
                )))
            }
        }
        let new = work
            .join(&name)
            .join(format!("giong{}", std::env::consts::EXE_SUFFIX));
        replace(exe, &new).map_err(|e| {
            Download::Failed(failed(
                format!("{} could not be replaced: {e}", exe.display()),
                format!(
                    "check that you may write to {}",
                    exe.parent().unwrap_or(exe).display()
                ),
            ))
        })
    })();
    let _ = fs::remove_dir_all(&work);
    result
}

/// The name a replaced Windows binary is moved to.
fn old_name(exe: &Path) -> PathBuf {
    exe.with_file_name(format!("giong.old{}", std::env::consts::EXE_SUFFIX))
}

/// Puts `new` where `exe` is, in one rename.
fn replace(exe: &Path, new: &Path) -> std::io::Result<()> {
    let staged = exe.with_file_name(format!("giong.new{}", std::env::consts::EXE_SUFFIX));
    let _ = fs::remove_file(&staged);
    fs::copy(new, &staged)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&staged, fs::Permissions::from_mode(0o755))?;
    }
    if cfg!(windows) {
        let old = old_name(exe);
        let _ = fs::remove_file(&old);
        fs::rename(exe, &old)?;
        if let Err(e) = fs::rename(&staged, exe) {
            let _ = fs::rename(&old, exe);
            return Err(e);
        }
        Ok(())
    } else {
        fs::rename(&staged, exe).inspect_err(|_| {
            let _ = fs::remove_file(&staged);
        })
    }
}

fn cargo_install(exe: &Path, target: Version, root: Option<&Path>) -> Result<(), Failure> {
    let mut cmd = crate::project::cargo();
    cmd.args(["install", "giong-cli", "--locked", "--version"])
        .arg(target.to_string());
    if let Some(r) = root {
        cmd.arg("--root").arg(r);
    }
    eprintln!("giong: cargo install giong-cli --locked --version {target} (a few minutes)");
    // A running program cannot be replaced on Windows: move it aside first.
    let moved = cfg!(windows) && {
        let _ = fs::remove_file(old_name(exe));
        fs::rename(exe, old_name(exe)).is_ok()
    };
    let status = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::from(std::io::stderr()))
        .status();
    let ok = matches!(&status, Ok(s) if s.success());
    if !ok && moved && !exe.exists() {
        let _ = fs::rename(old_name(exe), exe);
    }
    match status {
        Ok(s) if s.success() => Ok(()),
        Ok(_) => Err(failed(
            "cargo install giong-cli failed",
            "read cargo's error above; cargo install giong-cli --locked --version x.y.z runs \
             the same by hand",
        )),
        Err(e) => Err(crate::project::cargo_missing(e)),
    }
}

/// The version the binary at `exe` reports, after a replacement.
pub fn installed_version(exe: &Path) -> Option<Version> {
    let out = host::run(Command::new(exe).arg("--version")).ok()?;
    host::parse_version(out.trim().trim_start_matches("giong"))
}

/// Removes what an earlier Windows upgrade moved aside.
pub fn tidy(exe: &Path) {
    if cfg!(windows) {
        let _ = fs::remove_file(old_name(exe));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_https_download_never_follows_a_redirect_to_http() {
        let args = https_only("https://github.com/giong-run/giong/releases/download/v1.5.1/x");
        let pair = |flag: &str| args.iter().position(|a| *a == flag).map(|i| args[i + 1]);
        assert_eq!(pair("--proto"), Some("=https"));
        assert_eq!(pair("--proto-redir"), Some("=https"));
        assert!(args.contains(&"--tlsv1.2"));
        // A mirror the user named over http (a local one) still works.
        assert!(https_only("http://127.0.0.1:8765/dist/v1.5.1/x").is_empty());
    }

    #[test]
    fn the_install_is_told_by_its_folder() {
        // Paths that do not exist compare as written.
        std::env::set_var("GIONG_INSTALL", "/nowhere/giong-test-install");
        assert_eq!(
            detect(Path::new("/nowhere/giong-test-install/giong")),
            Method::Script {
                dir: "/nowhere/giong-test-install".into()
            }
        );
        std::env::remove_var("GIONG_INSTALL");
        assert!(matches!(
            detect(Path::new("/opt/homebrew-test/bin/giong")),
            Method::Unknown { .. }
        ));
        if let Some(h) = home() {
            let cargo_bin = std::env::var_os("CARGO_HOME")
                .map(PathBuf::from)
                .unwrap_or_else(|| h.join(".cargo"))
                .join("bin");
            assert!(
                matches!(detect(&cargo_bin.join("giong")), Method::Cargo { .. }),
                "{}",
                cargo_bin.display()
            );
        }
    }

    #[test]
    fn the_published_targets_match_the_install_scripts() {
        let here = Path::new(env!("CARGO_MANIFEST_DIR")).join("../site/public");
        let (Ok(sh), Ok(ps)) = (
            fs::read_to_string(here.join("install.sh")),
            fs::read_to_string(here.join("install.ps1")),
        ) else {
            return; // the published crate has no site
        };
        for t in [
            "unknown-linux-musl",
            "apple-darwin",
            "giong-${tag#v}-$t",
            "$base/$name.tar.gz.sha256",
        ] {
            assert!(sh.contains(t), "install.sh no longer has {t}");
        }
        for t in [
            "x86_64-pc-windows-msvc",
            "$name.zip.sha256",
            "giong-$version-$target",
        ] {
            assert!(ps.contains(t), "install.ps1 no longer has {t}");
        }
    }
}
