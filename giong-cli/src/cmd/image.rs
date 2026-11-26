//! `giong image`: an OCI container image of the service without Docker or
//! a Dockerfile. One layer: the release binary at `/app/<name>`, this
//! machine's CA certificates, a `nonroot` user (65532), `/tmp`, the files
//! given with `--add`, and, for a glibc binary, the libraries it loads. No
//! shell, no package manager: what distroless images hold, built here.
//!
//! The image is reproducible: entries are sorted, owned by 0 (or 65532),
//! dated `SOURCE_DATE_EPOCH` or the commit's time, so the same source gives
//! the same digest. It is written as an OCI archive that `docker load`,
//! `podman load`, `skopeo` and `crane` read; `--load` and `--push` hand it
//! to them.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::{Component, Path, PathBuf};
use std::process::Command;

use serde_json::json;

use super::Command as Cmd;
use crate::args::{Args, Flag, Kind};
use crate::code::Code;
use crate::project::{find_manifest_dir, Project};
use crate::report::{Failure, Finding, Report, Severity};
use crate::sha256;

pub const COMMAND: Cmd = Cmd {
    name: "image",
    summary: "build an OCI container image of the service, without Docker",
    usage: "giong image [--tag name:tag] [--add static,migrations] [--port 8080] [--out <file>]\n\
            giong image --load\n\
            giong image --push ghcr.io/owner/name:tag\n\n\
            Builds the service in release mode for Linux (static with musl when that target is \
            installed, else glibc with this machine's libraries) and writes a one-layer OCI \
            archive: the binary at /app, CA certificates, a nonroot user, no shell. The same \
            source gives the same image digest. --load hands it to docker or podman; --push \
            sends it with skopeo, crane, docker or podman.",
    flags: &[
        Flag {
            name: "tag",
            kind: Kind::Value,
            help: "the image name and tag (default: the package's name and version)",
        },
        Flag {
            name: "add",
            kind: Kind::Value,
            help: "files or directories of the project to put under /app, comma-separated",
        },
        Flag {
            name: "port",
            kind: Kind::Value,
            help: "the port the image says it listens on (default: LISTEN in .env.example, else 8080)",
        },
        Flag {
            name: "target",
            kind: Kind::Value,
            help: "the Rust target to build for (default: musl for this architecture if installed, else this machine's)",
        },
        Flag {
            name: "bin",
            kind: Kind::Value,
            help: "the binary when the package builds more than one",
        },
        Flag {
            name: "out",
            kind: Kind::Value,
            help: "where to write the archive (default <target dir>/giong-image/<name>-<tag>.tar)",
        },
        Flag {
            name: "load",
            kind: Kind::Switch,
            help: "load the image into docker or podman afterwards",
        },
        Flag {
            name: "push",
            kind: Kind::Value,
            help: "push the image to this reference afterwards (registry/name:tag)",
        },
    ],
    max_words: 0,
    run,
};

/// The id of the user the service runs as (distroless's `nonroot`).
pub const NONROOT: u32 = 65532;

/// An image reference split into name and tag; checks the OCI grammar.
pub fn parse_ref(r: &str) -> Result<(String, String), Failure> {
    let bad = |why: &str| {
        Failure::new(
            Code::IMAGE_TAG,
            format!("{r:?} is not an image reference: {why}"),
            "write name:tag in lower case, such as myservice:1.2.0 or ghcr.io/owner/myservice:1.2.0",
        )
    };
    // The tag is after the last ':' that comes after the last '/' (a
    // registry's port has a ':' too).
    let slash = r.rfind('/').map_or(0, |i| i + 1);
    let (name, tag) = match r[slash..].rfind(':') {
        Some(i) => (&r[..slash + i], &r[slash + i + 1..]),
        None => (r, "latest"),
    };
    if r.contains('@') {
        return Err(bad("a digest (@sha256:...) cannot be written to"));
    }
    if tag.is_empty()
        || tag.len() > 128
        || tag.starts_with(['.', '-'])
        || !tag
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_.-".contains(&b))
    {
        return Err(bad(
            "the tag is up to 128 letters, digits, _ . -, not starting with . or -",
        ));
    }
    let mut parts: Vec<&str> = name.split('/').collect();
    // A first part with a '.' or ':' or "localhost" is a registry host.
    if parts.len() > 1 && (parts[0].contains(['.', ':']) || parts[0] == "localhost") {
        let host = parts.remove(0);
        if !host
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b".-:".contains(&b))
        {
            return Err(bad("the registry host has characters a host name cannot"));
        }
    }
    let component_ok = |c: &str| {
        let b = c.as_bytes();
        !b.is_empty()
            && b.first()
                .is_some_and(|x| x.is_ascii_lowercase() || x.is_ascii_digit())
            && b.last()
                .is_some_and(|x| x.is_ascii_lowercase() || x.is_ascii_digit())
            && b.iter()
                .all(|x| x.is_ascii_lowercase() || x.is_ascii_digit() || b"._-".contains(x))
            && !c.contains("..")
    };
    if parts.is_empty() || !parts.iter().all(|c| component_ok(c)) {
        return Err(bad("the name is lower-case letters and digits, with . _ - between them and / between parts"));
    }
    Ok((name.to_string(), tag.to_string()))
}

/// A package name as an image name: lower case, `_` kept.
pub fn image_name(package: &str) -> String {
    let s: String = package
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.' {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect();
    let s = s
        .trim_matches(|c: char| !c.is_ascii_alphanumeric())
        .to_string();
    if s.is_empty() {
        "app".into()
    } else {
        s
    }
}

/// One file or directory in the layer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    /// Without a leading `/`; directories end without `/`.
    pub path: String,
    pub mode: u32,
    pub uid: u32,
    /// `None` for a directory.
    pub data: Option<Vec<u8>>,
}

impl Entry {
    fn file(path: &str, mode: u32, data: Vec<u8>) -> Entry {
        Entry {
            path: path.trim_start_matches('/').to_string(),
            mode,
            uid: 0,
            data: Some(data),
        }
    }

    fn dir(path: &str, mode: u32, uid: u32) -> Entry {
        Entry {
            path: path
                .trim_start_matches('/')
                .trim_end_matches('/')
                .to_string(),
            mode,
            uid,
            data: None,
        }
    }
}

fn octal(field: &mut [u8], v: u64) {
    let s = format!("{v:0width$o}", width = field.len() - 1);
    let b = s.as_bytes();
    let b = &b[b.len().saturating_sub(field.len() - 1)..];
    field[..b.len()].copy_from_slice(b);
    field[field.len() - 1] = 0;
}

/// A ustar archive of `entries`, sorted by path, every parent directory
/// added, all dated `mtime`: the same entries give the same bytes.
pub fn tar(entries: &[Entry], mtime: u64) -> Result<Vec<u8>, String> {
    let mut all: BTreeMap<String, Entry> = BTreeMap::new();
    for e in entries {
        for parent in Path::new(&e.path).ancestors().skip(1) {
            if parent.as_os_str().is_empty() {
                break;
            }
            let p = parent.to_string_lossy().replace('\\', "/");
            all.entry(p.clone())
                .or_insert_with(|| Entry::dir(&p, 0o755, 0));
        }
        all.insert(e.path.clone(), e.clone());
    }
    let mut out = Vec::new();
    for e in all.values() {
        let name = if e.data.is_none() {
            format!("{}/", e.path)
        } else {
            e.path.clone()
        };
        let mut h = [0u8; 512];
        let (prefix, short) = if name.len() <= 100 {
            ("", name.as_str())
        } else {
            // Split at a '/' so the prefix is at most 155 bytes and the rest
            // at most 100.
            // (Not at the '/' that ends a directory's name.) Looked for
            // among the bytes: a byte count can fall inside a character of
            // a name that is not ASCII, and a '/' byte never does.
            let cut = name.as_bytes()[..(name.len() - 1).min(156)]
                .iter()
                .rposition(|&b| b == b'/')
                .filter(|&i| name.len() - i - 1 <= 100)
                .ok_or_else(|| format!("a path longer than a tar header holds: {name}"))?;
            (&name[..cut], &name[cut + 1..])
        };
        h[..short.len()].copy_from_slice(short.as_bytes());
        octal(&mut h[100..108], u64::from(e.mode));
        octal(&mut h[108..116], u64::from(e.uid));
        octal(&mut h[116..124], u64::from(e.uid));
        octal(
            &mut h[124..136],
            e.data.as_ref().map_or(0, |d| d.len() as u64),
        );
        octal(&mut h[136..148], mtime);
        h[156] = if e.data.is_none() { b'5' } else { b'0' };
        h[257..263].copy_from_slice(b"ustar\0");
        h[263..265].copy_from_slice(b"00");
        h[345..345 + prefix.len()].copy_from_slice(prefix.as_bytes());
        h[148..156].copy_from_slice(b"        ");
        let sum: u32 = h.iter().map(|&b| u32::from(b)).sum();
        let s = format!("{sum:06o}\0 ");
        h[148..156].copy_from_slice(s.as_bytes());
        out.extend_from_slice(&h);
        if let Some(d) = &e.data {
            out.extend_from_slice(d);
            out.resize(out.len().div_ceil(512) * 512, 0);
        }
    }
    out.resize(out.len() + 1024, 0);
    Ok(out)
}

/// `seconds` since 1970 as RFC 3339 UTC.
pub fn rfc3339(seconds: u64) -> String {
    let days = (seconds / 86_400) as i64;
    let rem = seconds % 86_400;
    // Howard Hinnant's civil_from_days.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

/// The ELF facts the image needs: its architecture (OCI name) and whether
/// it asks for a dynamic loader.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Elf {
    pub arch: &'static str,
    pub dynamic: bool,
}

pub fn read_elf(b: &[u8]) -> Result<Elf, &'static str> {
    if b.len() < 64 || &b[..4] != b"\x7fELF" {
        return Err("not an ELF binary (an image runs Linux binaries)");
    }
    if b[4] != 2 || b[5] != 1 {
        return Err("not a 64-bit little-endian ELF binary");
    }
    let u16_at = |i: usize| u16::from_le_bytes([b[i], b[i + 1]]);
    let u64_at = |i: usize| {
        let mut a = [0u8; 8];
        a.copy_from_slice(&b[i..i + 8]);
        u64::from_le_bytes(a)
    };
    let arch = match u16_at(18) {
        0x3e => "amd64",
        0xb7 => "arm64",
        0xf3 => "riscv64",
        _ => return Err("an architecture images are not built for here"),
    };
    let phoff = u64_at(32) as usize;
    let phentsize = u16_at(54) as usize;
    let phnum = u16_at(56) as usize;
    let mut dynamic = false;
    for i in 0..phnum {
        let at = phoff + i * phentsize;
        if at + 4 > b.len() {
            return Err("a truncated ELF binary");
        }
        // PT_INTERP: the binary needs a dynamic loader.
        if u32::from_le_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]]) == 3 {
            dynamic = true;
        }
    }
    Ok(Elf { arch, dynamic })
}

/// The libraries `ldd` lists for a binary: (path in the image, file to read).
pub fn parse_ldd(out: &str) -> Result<Vec<String>, String> {
    let mut libs = Vec::new();
    for line in out.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with("linux-vdso") || line.starts_with("linux-gate") {
            continue;
        }
        if line.contains("not found") {
            return Err(line.to_string());
        }
        let path = match line.split_once("=>") {
            Some((_, rest)) => rest.trim(),
            None => line,
        };
        let path = path.split(" (").next().unwrap_or("").trim();
        if path.starts_with('/') {
            libs.push(path.to_string());
        }
    }
    Ok(libs)
}

fn ca_bundle() -> Option<Vec<u8>> {
    let from_env = std::env::var_os("SSL_CERT_FILE").map(PathBuf::from);
    from_env
        .into_iter()
        .chain(
            [
                "/etc/ssl/certs/ca-certificates.crt",
                "/etc/pki/tls/certs/ca-bundle.crt",
                "/etc/ssl/cert.pem",
                "/etc/ssl/ca-bundle.pem",
                "/opt/homebrew/etc/ca-certificates/cert.pem",
                "/usr/local/etc/ca-certificates/cert.pem",
            ]
            .map(PathBuf::from),
        )
        .find_map(|p| {
            std::fs::read(p)
                .ok()
                .filter(|b| b.windows(11).any(|w| w == b"BEGIN CERTI"))
        })
}

/// The port from `LISTEN` in `.env.example`, even commented out
/// (`# LISTEN=0.0.0.0:3000`).
pub fn port_from_example(text: &str) -> Option<u16> {
    text.lines()
        .map(|l| l.trim().trim_start_matches('#').trim())
        .find_map(|l| l.strip_prefix("LISTEN="))
        .and_then(|v| {
            v.trim()
                .trim_matches(['"', '\''])
                .rsplit(':')
                .next()?
                .parse()
                .ok()
        })
}

/// Files under `rel` (a file or a directory) in `root`, sorted.
fn collect(root: &Path, rel: &str, out: &mut Vec<Entry>) -> Result<(), Failure> {
    let bad = |what: String| {
        Failure::new(
            Code::IMAGE_ADD,
            what,
            "give paths inside the project, comma-separated: giong image --add static,migrations",
        )
    };
    let p = Path::new(rel);
    if rel.is_empty()
        || p.is_absolute()
        || p.components()
            .any(|c| !matches!(c, Component::Normal(_) | Component::CurDir))
    {
        return Err(bad(format!("{rel:?} is not a path inside the project")));
    }
    let full = root.join(p);
    let mut meta = std::fs::symlink_metadata(&full).map_err(|e| bad(format!("{rel}: {e}")))?;
    // A cloned repository can commit a link to anything on this machine
    // (static/x -> ~/.ssh/id_rsa, or static -> ~ with --add static/x): every
    // entry must resolve inside the project, a link only to a file there,
    // which goes in as a copy of that file.
    let target = std::fs::canonicalize(&full).map_err(|e| bad(format!("{rel}: {e}")))?;
    let inside = std::fs::canonicalize(root).is_ok_and(|r| target.starts_with(r));
    let link = meta.file_type().is_symlink();
    if link {
        meta = std::fs::metadata(&target).map_err(|e| bad(format!("{rel}: {e}")))?;
    }
    if !inside || (link && !meta.is_file()) {
        return Err(Failure::new(
            Code::IMAGE_ADD,
            format!(
                "{rel} {} {}, {}",
                if link { "is a link to" } else { "resolves to" },
                target.display(),
                if inside {
                    "a folder"
                } else {
                    "outside the project"
                }
            ),
            "giong image adds a link only when it points at a file inside the project: replace the link with the files, or leave it out of --add",
        ));
    }
    let clean: String = p
        .components()
        .filter_map(|c| match c {
            Component::Normal(s) => Some(s.to_string_lossy().into_owned()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("/");
    if meta.is_dir() {
        let mut names = Vec::new();
        for e in std::fs::read_dir(&full).map_err(|e| bad(format!("{rel}: {e}")))? {
            let name = e.map_err(|e| bad(format!("{rel}: {e}")))?.file_name();
            // Written as UTF-8 in the image: a name that is not would be
            // renamed there (with U+FFFD), and the service would not find
            // its file.
            let name = name.into_string().map_err(|name| {
                Failure::new(
                    Code::IMAGE_ADD,
                    format!(
                        "{rel}/{} has a name that is not UTF-8",
                        name.to_string_lossy()
                    ),
                    "rename the file to a UTF-8 name, or leave its folder out of --add",
                )
            })?;
            names.push(name);
        }
        names.sort();
        out.push(Entry::dir(&format!("app/{clean}"), 0o755, 0));
        for n in names {
            collect(root, &format!("{clean}/{n}"), out)?;
        }
    } else if meta.is_file() {
        let data = std::fs::read(&target).map_err(|e| bad(format!("{rel}: {e}")))?;
        #[cfg(unix)]
        let mode = {
            use std::os::unix::fs::PermissionsExt;
            if meta.permissions().mode() & 0o111 != 0 {
                0o755
            } else {
                0o644
            }
        };
        #[cfg(not(unix))]
        let mode = 0o644;
        out.push(Entry::file(&format!("app/{clean}"), mode, data));
    }
    Ok(())
}

/// The time the image is dated: `SOURCE_DATE_EPOCH`, else the commit's
/// time, else 0.
fn source_date(dir: &Path) -> u64 {
    if let Some(t) = std::env::var("SOURCE_DATE_EPOCH")
        .ok()
        .and_then(|v| v.trim().parse().ok())
    {
        return t;
    }
    Command::new("git")
        .args(["log", "-1", "--format=%ct"])
        .current_dir(dir)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8_lossy(&o.stdout).trim().parse().ok())
        .unwrap_or(0)
}

fn git_out(dir: &Path, args: &[&str]) -> Option<String> {
    Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|s| !s.is_empty())
}

/// What goes into the image besides the layer's files.
pub struct Meta {
    pub name: String,
    pub tag: String,
    pub arch: &'static str,
    pub binary: String,
    pub port: u16,
    pub created: u64,
    pub labels: BTreeMap<String, String>,
}

/// A reference as containerd stores it: `svc:1` is
/// `docker.io/library/svc:1`, `owner/svc:1` is `docker.io/owner/svc:1`
/// (Docker's containerd store finds only the qualified form).
pub fn qualified(r: &str) -> String {
    let first = r.split('/').next().unwrap_or("");
    let has_host = r.contains('/') && (first.contains(['.', ':']) || first == "localhost");
    if has_host {
        r.to_string()
    } else if r.contains('/') {
        format!("docker.io/{r}")
    } else {
        format!("docker.io/library/{r}")
    }
}

/// The archive's files: (path in the archive, bytes), and the manifest's
/// digest (the image's id in a registry).
pub fn oci(layer: &[u8], m: &Meta) -> (Vec<(String, Vec<u8>)>, String) {
    let layer_digest = format!("sha256:{}", sha256::hex(layer));
    let created = rfc3339(m.created);
    let mut env = vec!["PATH=/usr/local/bin:/usr/bin:/bin".to_string()];
    env.push("SSL_CERT_FILE=/etc/ssl/certs/ca-certificates.crt".into());
    // A service from `giong new` listens on 127.0.0.1 without LISTEN, which
    // inside a container no published port reaches. The Dockerfile it comes
    // with sets the same; `docker run -e LISTEN=...` still wins.
    env.push(format!("LISTEN=0.0.0.0:{}", m.port));
    let config = json!({
        "architecture": m.arch,
        "os": "linux",
        "created": created,
        "config": {
            "Entrypoint": [format!("/app/{}", m.binary)],
            "WorkingDir": "/app",
            "User": format!("{NONROOT}:{NONROOT}"),
            "Env": env,
            "ExposedPorts": { format!("{}/tcp", m.port): {} },
            "Labels": m.labels,
        },
        "rootfs": { "type": "layers", "diff_ids": [layer_digest] },
        "history": [{ "created": created, "created_by": "giong image" }],
    })
    .to_string()
    .into_bytes();
    let config_digest = format!("sha256:{}", sha256::hex(&config));
    let manifest = json!({
        "schemaVersion": 2,
        "mediaType": "application/vnd.oci.image.manifest.v1+json",
        "config": {
            "mediaType": "application/vnd.oci.image.config.v1+json",
            "digest": config_digest, "size": config.len(),
        },
        "layers": [{
            "mediaType": "application/vnd.oci.image.layer.v1.tar",
            "digest": layer_digest, "size": layer.len(),
        }],
        "annotations": { "org.opencontainers.image.created": created },
    })
    .to_string()
    .into_bytes();
    let manifest_digest = format!("sha256:{}", sha256::hex(&manifest));
    let full = format!("{}:{}", m.name, m.tag);
    let index = json!({
        "schemaVersion": 2,
        "mediaType": "application/vnd.oci.image.index.v1+json",
        "manifests": [{
            "mediaType": "application/vnd.oci.image.manifest.v1+json",
            "digest": manifest_digest, "size": manifest.len(),
            "platform": { "architecture": m.arch, "os": "linux" },
            "annotations": {
                "org.opencontainers.image.ref.name": m.tag,
                "io.containerd.image.name": qualified(&full),
            },
        }],
    })
    .to_string()
    .into_bytes();
    let blob = |d: &str| format!("blobs/sha256/{}", d.trim_start_matches("sha256:"));
    // manifest.json: the Docker archive format, so older `docker load`
    // reads the same file.
    let docker = json!([{
        "Config": blob(&config_digest),
        "RepoTags": [full],
        "Layers": [blob(&layer_digest)],
    }])
    .to_string()
    .into_bytes();
    let files = vec![
        (
            "oci-layout".to_string(),
            br#"{"imageLayoutVersion":"1.0.0"}"#.to_vec(),
        ),
        ("index.json".to_string(), index),
        ("manifest.json".to_string(), docker),
        (blob(&config_digest), config),
        (blob(&manifest_digest), manifest),
        (blob(&layer_digest), layer.to_vec()),
    ];
    (files, manifest_digest)
}

fn tool(name: &str) -> bool {
    Command::new(name)
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success())
}

fn runtime() -> Option<String> {
    let forced = std::env::var("GIONG_CONTAINER_RUNTIME")
        .ok()
        .filter(|s| !s.is_empty());
    let candidates = forced.map_or_else(
        || vec!["docker".to_string(), "podman".to_string()],
        |f| vec![f],
    );
    candidates.into_iter().find(|c| {
        Command::new(c)
            .args(["info", "--format", "{{.ServerVersion}}"])
            .output()
            .is_ok_and(|o| o.status.success())
    })
}

fn run_tool(cmd: &str, args: &[&str]) -> Result<(), String> {
    let out = Command::new(cmd)
        .args(args)
        .output()
        .map_err(|e| format!("{cmd} could not be started: {e}"))?;
    if out.status.success() {
        Ok(())
    } else {
        let err = String::from_utf8_lossy(&out.stderr);
        let first = err
            .lines()
            .rev()
            .find(|l| !l.trim().is_empty())
            .unwrap_or("no output");
        Err(format!(
            "{cmd} {} failed: {}",
            args.first().unwrap_or(&""),
            first.trim()
        ))
    }
}

/// The Rust target to build for, and the cargo subcommand.
fn choose_target(flag: Option<&str>) -> Result<(Option<String>, &'static str), Failure> {
    let installed = || {
        Command::new("rustup")
            .args(["target", "list", "--installed"])
            .output()
            .ok()
            .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
            .unwrap_or_default()
    };
    let linux = cfg!(target_os = "linux");
    let musl = format!("{}-unknown-linux-musl", std::env::consts::ARCH);
    let zig = || {
        Command::new(std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into()))
            .args(["zigbuild", "--help"])
            .output()
            .is_ok_and(|o| o.status.success())
    };
    if let Some(t) = flag {
        if !t.contains("linux") {
            return Err(Failure::new(
                Code::IMAGE_TARGET,
                format!("--target {t} is not a Linux target"),
                "give a Linux target, such as x86_64-unknown-linux-musl or aarch64-unknown-linux-musl",
            ));
        }
        let native = linux && t.starts_with(std::env::consts::ARCH);
        return Ok((
            Some(t.to_string()),
            if native || !zig() {
                "build"
            } else {
                "zigbuild"
            },
        ));
    }
    let have_musl = installed().lines().any(|l| l.trim() == musl);
    if linux {
        return Ok(if have_musl {
            (Some(musl), "build")
        } else {
            (None, "build")
        });
    }
    if have_musl && zig() {
        return Ok((Some(musl), "zigbuild"));
    }
    Err(Failure::new(
        Code::IMAGE_TARGET,
        format!("this machine ({}) builds no Linux binary yet", std::env::consts::OS),
        format!("rustup target add {musl} and cargo install cargo-zigbuild (it needs Zig), then run giong image again"),
    ))
}

fn run(args: &Args) -> Result<Report, Failure> {
    let cwd = std::env::current_dir().map_err(|e| {
        Failure::new(
            Code::NOT_A_PROJECT,
            format!("cannot read the current directory: {e}"),
            "run giong image inside a project",
        )
    })?;
    let root = find_manifest_dir(&cwd).ok_or_else(|| {
        Failure::new(
            Code::NOT_A_PROJECT,
            format!("no Cargo.toml in {} or above it", cwd.display()),
            "run giong image inside the service's project",
        )
    })?;
    // Every flag is checked before cargo runs: a typo fails at once.
    let push = args.value("push");
    let explicit = match (args.value("tag"), push) {
        (Some(t), _) => Some(parse_ref(t)?),
        (None, Some(p)) => Some(parse_ref(p)?),
        (None, None) => None,
    };
    if let Some(p) = push {
        parse_ref(p)?;
    }
    let port_flag = match args.number::<u16>("port")? {
        Some(0) => {
            return Err(Failure::new(
                Code::USAGE,
                "--port 0 is not a port",
                "give the port the service listens on, such as --port 8080",
            ))
        }
        other => other,
    };
    let mut added = Vec::new();
    if let Some(list) = args.value("add") {
        for rel in list.split(',').map(str::trim).filter(|s| !s.is_empty()) {
            collect(&root, rel, &mut added)?;
        }
    }
    let (target, sub) = choose_target(args.value("target"))?;

    let project = Project::at(&cwd)?;
    let manifest =
        std::fs::canonicalize(root.join("Cargo.toml")).unwrap_or_else(|_| root.join("Cargo.toml"));
    let package = project
        .packages()
        .find(|p| {
            p["manifest_path"].as_str().is_some_and(|m| {
                std::fs::canonicalize(m)
                    .map(|m| m == manifest)
                    .unwrap_or(false)
            })
        })
        .cloned();
    let pkg_name = package
        .as_ref()
        .and_then(|p| p["name"].as_str())
        .unwrap_or("app")
        .to_string();
    let pkg_version = package
        .as_ref()
        .and_then(|p| p["version"].as_str())
        .unwrap_or("latest")
        .to_string();

    let (name, tag) = match explicit {
        Some(r) => r,
        None => parse_ref(&format!(
            "{}:{}",
            image_name(&pkg_name),
            pkg_version.replace('+', "_")
        ))?,
    };
    let port = port_flag.unwrap_or_else(|| {
        crate::state::read_to_string(&root.join(".env.example"))
            .ok()
            .and_then(|t| port_from_example(&t))
            .unwrap_or(8080)
    });
    let mut extra = Vec::new();
    if let Some(t) = &target {
        extra.push("--target".to_string());
        extra.push(t.clone());
    }
    let quiet = args.json();
    let mut notes: Vec<String> = Vec::new();
    let auto_musl =
        args.value("target").is_none() && target.as_deref().is_some_and(|t| t.contains("musl"));
    let mut built =
        super::bench::build_release(&cwd, sub, &extra, args.value("bin"), quiet, "giong image");
    let mut target = target;
    if auto_musl && cfg!(target_os = "linux") {
        if let Err(f) = &built {
            if f.code == Code::CARGO_FAILED {
                // A musl build giong chose by itself failed (a dependency
                // that does not build for musl): glibc and this machine's
                // libraries instead.
                notes.push(format!("the static musl build failed ({}), so the image uses glibc and this machine's libraries", f.what));
                target = None;
                built = super::bench::build_release(
                    &cwd,
                    "build",
                    &[],
                    args.value("bin"),
                    quiet,
                    "giong image",
                );
            }
        }
    }
    let built = built.map_err(|f| {
        if target.as_deref().is_some_and(|t| t.contains("musl")) && f.code == Code::CARGO_FAILED {
            Failure::new(
                Code::IMAGE_TARGET,
                format!("the musl build failed: {}", f.what),
                "fix what cargo reports for the musl target, or build against glibc: leave out --target on Linux",
            )
        } else {
            f
        }
    })?;
    let binary = std::fs::read(&built).map_err(|e| {
        Failure::new(
            Code::IMAGE_TARGET,
            format!("cannot read {}: {e}", built.display()),
            "build again: cargo build --release",
        )
    })?;
    let elf = read_elf(&binary).map_err(|why| {
        Failure::new(
            Code::IMAGE_TARGET,
            format!("{} is {why}", built.display()),
            "build for Linux: giong image picks a Linux target by itself on Linux; elsewhere install the musl target and cargo-zigbuild",
        )
    })?;
    let bin_name = built
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .into_owned();

    let mut r = Report::new("image");
    for n in &notes {
        r.line(format!("Note: {n}."));
    }
    let mut entries = vec![
        Entry::file(&format!("app/{bin_name}"), 0o755, binary.clone()),
        Entry::dir("app", 0o755, 0),
        Entry::dir("tmp", 0o1777, 0),
        Entry::dir("home/nonroot", 0o700, NONROOT),
        Entry::file(
            "etc/passwd",
            0o644,
            format!("root:x:0:0:root:/root:/sbin/nologin\nnonroot:x:{NONROOT}:{NONROOT}:nonroot:/home/nonroot:/sbin/nologin\n").into_bytes(),
        ),
        Entry::file("etc/group", 0o644, format!("root:x:0:\nnonroot:x:{NONROOT}:\n").into_bytes()),
        Entry::file("etc/nsswitch.conf", 0o644, b"hosts: files dns\n".to_vec()),
    ];
    match ca_bundle() {
        Some(ca) => entries.push(Entry::file("etc/ssl/certs/ca-certificates.crt", 0o644, ca)),
        None => r.push(
            Finding::new(Code::IMAGE_NO_CERTS, Severity::Warning, "the image has no CA certificates")
                .detail("outgoing TLS connections (HTTPS APIs, databases over TLS) will fail to verify")
                .fix("install the ca-certificates package, or set SSL_CERT_FILE to a PEM bundle, and run giong image again"),
        ),
    }
    let mut libs = Vec::new();
    if elf.dynamic {
        let out = Command::new("ldd").arg(&built).output().map_err(|e| {
            Failure::new(
                Code::IMAGE_LIBRARIES,
                format!("the binary is linked dynamically and ldd could not be run: {e}"),
                format!("rustup target add {}-unknown-linux-musl for a static binary, or install ldd (libc-bin)", std::env::consts::ARCH),
            )
        })?;
        libs = parse_ldd(&String::from_utf8_lossy(&out.stdout)).map_err(|missing| {
            Failure::new(
                Code::IMAGE_LIBRARIES,
                format!("the binary needs a library this machine lacks: {missing}"),
                "install it, or build statically: rustup target add x86_64-unknown-linux-musl",
            )
        })?;
        for lib in &libs {
            let data = std::fs::canonicalize(lib)
                .and_then(std::fs::read)
                .map_err(|e| {
                    Failure::new(
                        Code::IMAGE_LIBRARIES,
                        format!("cannot read {lib}: {e}"),
                        "check the library ldd names",
                    )
                })?;
            entries.push(Entry::file(lib, 0o755, data));
        }
    }
    entries.extend(added.iter().cloned());

    let created = source_date(&root);
    let layer = tar(&entries, created)
        .map_err(|e| Failure::new(Code::IMAGE_ADD, e, "use shorter paths under --add"))?;
    let mut labels = BTreeMap::new();
    labels.insert(
        "org.opencontainers.image.title".to_string(),
        pkg_name.clone(),
    );
    labels.insert(
        "org.opencontainers.image.version".to_string(),
        pkg_version.clone(),
    );
    labels.insert(
        "org.opencontainers.image.created".to_string(),
        rfc3339(created),
    );
    if let Some(sha) = git_out(&root, &["rev-parse", "HEAD"]) {
        labels.insert("org.opencontainers.image.revision".to_string(), sha);
    }
    if let Some(src) = git_out(&root, &["remote", "get-url", "origin"])
        .filter(|u| u.starts_with("https://") && !u.contains('@'))
    {
        labels.insert("org.opencontainers.image.source".to_string(), src);
    }
    let meta = Meta {
        name: name.clone(),
        tag: tag.clone(),
        arch: elf.arch,
        binary: bin_name.clone(),
        port,
        created,
        labels,
    };
    let (files, digest) = oci(&layer, &meta);
    let archive_entries: Vec<Entry> = files
        .into_iter()
        .map(|(p, d)| Entry::file(&p, 0o644, d))
        .collect();
    let archive = tar(&archive_entries, created)
        .map_err(|e| Failure::new(Code::IMAGE_ADD, e, "report this as a bug"))?;
    let out = match args.value("out") {
        Some(o) => PathBuf::from(o),
        None => project.target_dir.join("giong-image").join(format!(
            "{}-{tag}.tar",
            name.rsplit('/').next().unwrap_or(&name)
        )),
    };
    if let Some(dir) = out.parent().filter(|d| !d.as_os_str().is_empty()) {
        let _ = std::fs::create_dir_all(dir);
    }
    std::fs::write(&out, &archive).map_err(|e| {
        Failure::new(
            Code::USAGE,
            format!("cannot write {}: {e}", out.display()),
            "give --out a writable path",
        )
    })?;

    let mib = |n: usize| format!("{:.1} MiB", n as f64 / (1024.0 * 1024.0));
    let kind = if elf.dynamic {
        format!("glibc, {} libraries from this machine", libs.len())
    } else {
        "static".to_string()
    };
    let full = format!("{name}:{tag}");
    r.line(format!(
        "{full}  linux/{}  {}  ({kind})",
        elf.arch,
        mib(layer.len())
    ));
    let mut line = format!("  /app/{bin_name} {}", mib(binary.len()));
    if !added.is_empty() {
        let _ = write!(
            line,
            ", {} added files",
            added.iter().filter(|e| e.data.is_some()).count()
        );
    }
    let _ = write!(
        line,
        "; port {port}; user nonroot ({NONROOT}); dated {}",
        rfc3339(created)
    );
    r.line(line);
    r.line(format!("  digest {digest}"));
    r.line(format!("Image: {}", out.display()));

    let mut loaded = None;
    if args.has("load") {
        let rt = runtime().ok_or_else(|| {
            Failure::new(
                Code::IMAGE_LOAD,
                "neither Docker nor Podman answers",
                format!(
                    "start one, or load the file yourself: docker load -i {}",
                    out.display()
                ),
            )
        })?;
        let path = out.display().to_string();
        run_tool(&rt, &["load", "-i", &path]).map_err(|e| {
            Failure::new(
                Code::IMAGE_LOAD,
                e,
                format!("Docker 25 or newer reads OCI archives; or try podman load -i {path}"),
            )
        })?;
        r.line(format!(
            "Loaded into {rt}: {rt} run --rm -p {port}:{port} {full}"
        ));
        loaded = Some(rt);
    } else {
        r.line(format!(
            "Run it: docker load -i {} && docker run --rm -p {port}:{port} {full}",
            out.display()
        ));
    }
    let mut pushed_with = None;
    if let Some(dest) = push {
        let path = out.display().to_string();
        let result = if tool("skopeo") {
            pushed_with = Some("skopeo");
            run_tool(
                "skopeo",
                &[
                    "copy",
                    &format!("oci-archive:{path}"),
                    &format!("docker://{dest}"),
                ],
            )
        } else if tool("crane") {
            pushed_with = Some("crane");
            run_tool("crane", &["push", &path, dest])
        } else if let Some(rt) = loaded.clone().or_else(runtime) {
            pushed_with = Some(if rt == "podman" { "podman" } else { "docker" });
            run_tool(&rt, &["load", "-i", &path])
                .and_then(|_| {
                    if dest == full {
                        Ok(())
                    } else {
                        run_tool(&rt, &["tag", &full, dest])
                    }
                })
                .and_then(|_| run_tool(&rt, &["push", dest]))
        } else {
            Err("none of skopeo, crane, docker or podman is installed".to_string())
        };
        result.map_err(|e| {
            Failure::new(Code::IMAGE_PUSH, format!("pushing {dest} failed: {e}"), format!("log in to the registry (docker login), or push the file yourself: skopeo copy oci-archive:{path} docker://{dest}"))
        })?;
        r.line(format!("Pushed {dest} with {}", pushed_with.unwrap_or("?")));
    }

    r.data("image", json!(full));
    r.data("digest", json!(digest));
    r.data("file", json!(out));
    r.data("architecture", json!(elf.arch));
    r.data("static", json!(!elf.dynamic));
    r.data("target", json!(target));
    r.data("libraries", json!(libs));
    r.data("binary_bytes", json!(binary.len()));
    r.data("layer_bytes", json!(layer.len()));
    r.data("port", json!(port));
    r.data("created", json!(rfc3339(created)));
    r.data("loaded", json!(loaded));
    r.data("pushed", json!(push));
    r.data("notes", json!(notes));
    Ok(r)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    #[test]
    fn references_parse() {
        let ok = |r: &str| parse_ref(r).unwrap();
        assert_eq!(ok("svc:1.2.0"), ("svc".into(), "1.2.0".into()));
        assert_eq!(ok("svc"), ("svc".into(), "latest".into()));
        assert_eq!(
            ok("ghcr.io/owner/svc:v1"),
            ("ghcr.io/owner/svc".into(), "v1".into())
        );
        assert_eq!(
            ok("localhost:5000/svc"),
            ("localhost:5000/svc".into(), "latest".into())
        );
        assert_eq!(
            ok("localhost:5000/a/b-c_d.e:x_Y.1"),
            ("localhost:5000/a/b-c_d.e".into(), "x_Y.1".into())
        );
        for bad in [
            "Svc:1",
            "svc:",
            "svc:-x",
            "svc:.x",
            "-svc",
            "svc-",
            "a//b",
            "svc@sha256:ab",
            "a..b",
            &format!("svc:{}", "x".repeat(129)),
        ] {
            assert_eq!(parse_ref(bad).unwrap_err().code, Code::IMAGE_TAG, "{bad}");
        }
        assert_eq!(image_name("Rest_API"), "rest_api");
        assert_eq!(image_name("--"), "app");
    }

    #[test]
    fn dates_are_rfc3339() {
        assert_eq!(rfc3339(0), "1970-01-01T00:00:00Z");
        assert_eq!(rfc3339(951_782_400), "2000-02-29T00:00:00Z");
        assert_eq!(rfc3339(1_795_348_319), "2026-11-22T11:51:59Z");
        assert_eq!(rfc3339(4_102_444_799), "2099-12-31T23:59:59Z");
    }

    #[test]
    fn ldd_output_parses() {
        let out = "\tlinux-vdso.so.1 (0x00007ffd)\n\tlibgcc_s.so.1 => /lib/x86_64-linux-gnu/libgcc_s.so.1 (0x1)\n\tlibc.so.6 => /lib/x86_64-linux-gnu/libc.so.6 (0x2)\n\t/lib64/ld-linux-x86-64.so.2 (0x3)\n";
        assert_eq!(
            parse_ldd(out).unwrap(),
            [
                "/lib/x86_64-linux-gnu/libgcc_s.so.1",
                "/lib/x86_64-linux-gnu/libc.so.6",
                "/lib64/ld-linux-x86-64.so.2"
            ]
        );
        assert!(parse_ldd("\tlibssl.so.3 => not found\n").is_err());
    }

    #[test]
    fn ports_come_from_the_example() {
        assert_eq!(port_from_example("# LISTEN=0.0.0.0:3000\n"), Some(3000));
        assert_eq!(port_from_example("A=1\nLISTEN=\"[::]:8443\"\n"), Some(8443));
        assert_eq!(port_from_example("A=1\n"), None);
    }

    #[test]
    fn this_binary_is_an_elf_on_linux() {
        if cfg!(target_os = "linux") {
            let me = std::fs::read(std::env::current_exe().unwrap()).unwrap();
            let elf = read_elf(&me).unwrap();
            assert_eq!(
                elf.arch,
                if cfg!(target_arch = "aarch64") {
                    "arm64"
                } else {
                    "amd64"
                }
            );
        }
        assert!(read_elf(b"MZ\x90\x00").is_err());
    }

    /// Reads a ustar archive back: (name, mode, uid, size, typeflag, data).
    fn untar(b: &[u8]) -> Vec<(String, u32, u32, char, Vec<u8>)> {
        let mut out = Vec::new();
        let mut at = 0;
        while at + 512 <= b.len() && b[at] != 0 {
            let h = &b[at..at + 512];
            let field = |r: std::ops::Range<usize>| {
                String::from_utf8_lossy(&h[r])
                    .trim_matches(|c| c == '\0' || c == ' ')
                    .to_string()
            };
            let num = |r: std::ops::Range<usize>| u64::from_str_radix(&field(r), 8).unwrap();
            let sum: u32 = h
                .iter()
                .enumerate()
                .map(|(i, &x)| {
                    if (148..156).contains(&i) {
                        32
                    } else {
                        u32::from(x)
                    }
                })
                .sum();
            assert_eq!(u64::from(sum), num(148..156), "checksum");
            let prefix = field(345..500);
            let name = if prefix.is_empty() {
                field(0..100)
            } else {
                format!("{prefix}/{}", field(0..100))
            };
            let size = num(124..136) as usize;
            let data = b[at + 512..at + 512 + size].to_vec();
            out.push((
                name,
                num(100..108) as u32,
                num(108..116) as u32,
                h[156] as char,
                data,
            ));
            at += 512 + size.div_ceil(512) * 512;
        }
        out
    }

    /// A cloned repository can commit `static/key -> ~/.ssh/id_rsa`: only a
    /// link to a file inside the project goes into the image.
    #[cfg(unix)]
    #[test]
    fn links_out_of_the_project_stay_out_of_the_image() {
        use std::os::unix::fs::symlink;
        let base = std::env::temp_dir().join(format!("giongimagelinks-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let root = base.join("proj");
        std::fs::create_dir_all(root.join("static")).unwrap();
        std::fs::write(base.join("id_rsa"), "secret").unwrap();
        std::fs::write(root.join("static/a.txt"), "a").unwrap();
        symlink("a.txt", root.join("static/b.txt")).unwrap();
        let mut out = Vec::new();
        collect(&root, "static", &mut out).unwrap();
        let paths: Vec<_> = out.iter().map(|e| e.path.as_str()).collect();
        assert_eq!(
            paths,
            ["app/static", "app/static/a.txt", "app/static/b.txt"]
        );

        symlink(base.join("id_rsa"), root.join("static/key")).unwrap();
        let e = collect(&root, "static", &mut Vec::new()).unwrap_err();
        assert_eq!(e.code, Code::IMAGE_ADD);
        assert!(e.what.contains("outside the project"), "{}", e.what);
        std::fs::remove_file(root.join("static/key")).unwrap();

        // A link to a folder is refused even inside: it can loop.
        symlink(".", root.join("static/loop")).unwrap();
        let e = collect(&root, "static", &mut Vec::new()).unwrap_err();
        assert!(e.what.contains("a folder"), "{}", e.what);
        std::fs::remove_file(root.join("static/loop")).unwrap();

        symlink(&base, root.join("up")).unwrap();
        assert!(collect(&root, "up", &mut Vec::new()).is_err());
        // A file reached through a linked folder outside is refused too.
        let e = collect(&root, "up/id_rsa", &mut Vec::new()).unwrap_err();
        assert!(e.what.contains("outside the project"), "{}", e.what);
        std::fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn tars_are_sorted_complete_and_reproducible() {
        let long = format!(
            "app/{}/{}/{}",
            "d".repeat(60),
            "e".repeat(60),
            "f".repeat(90)
        );
        let entries = vec![
            Entry::file("app/svc", 0o755, b"bin".to_vec()),
            Entry::dir("tmp", 0o1777, 0),
            Entry::file("etc/ssl/certs/ca.crt", 0o644, vec![b'x'; 600]),
            Entry::dir("home/nonroot", 0o700, NONROOT),
            Entry::file(&long, 0o644, vec![]),
        ];
        let a = tar(&entries, 1_700_000_000).unwrap();
        let mut rev = entries.clone();
        rev.reverse();
        assert_eq!(
            a,
            tar(&rev, 1_700_000_000).unwrap(),
            "order does not matter"
        );
        assert_eq!(a.len() % 512, 0);
        let back = untar(&a);
        let names: Vec<&str> = back.iter().map(|x| x.0.as_str()).collect();
        assert_eq!(
            names,
            [
                "app/",
                &format!("app/{}/", "d".repeat(60)),
                &format!("app/{}/{}/", "d".repeat(60), "e".repeat(60)),
                &long,
                "app/svc",
                "etc/",
                "etc/ssl/",
                "etc/ssl/certs/",
                "etc/ssl/certs/ca.crt",
                "home/",
                "home/nonroot/",
                "tmp/",
            ]
        );
        let get = |n: &str| back.iter().find(|x| x.0 == n).unwrap();
        assert_eq!(
            (
                get("app/svc").1,
                get("app/svc").3,
                get("app/svc").4.as_slice()
            ),
            (0o755, '0', &b"bin"[..])
        );
        assert_eq!((get("tmp/").1, get("tmp/").3), (0o1777, '5'));
        assert_eq!(get("home/nonroot/").2, NONROOT);
        assert_eq!(get("etc/ssl/certs/ca.crt").4.len(), 600);
        assert!(tar(&[Entry::file(&"x".repeat(300), 0o644, vec![])], 0).is_err());
    }

    /// Names that are not ASCII, in a path past a tar header's 100 bytes:
    /// the split looked for a '/' in a string cut at a byte count, which
    /// panicked when the count fell inside a character.
    #[test]
    fn long_paths_with_names_that_are_not_ascii() {
        // Byte 156 inside a character (each of these takes three bytes).
        let deep = format!("app/static/{}/xx{}.txt", "ả".repeat(20), "ấ".repeat(31));
        assert!(!deep.is_char_boundary(156));
        // The last byte inside one, past 100 bytes.
        let tail = format!("app/static/{}/x{}", "ả".repeat(15), "ấ".repeat(33));
        assert!(!tail.is_char_boundary(tail.len() - 1));
        let entries = [
            Entry::file(&deep, 0o644, b"a".to_vec()),
            Entry::file(&tail, 0o644, b"b".to_vec()),
        ];
        let back = untar(&tar(&entries, 0).unwrap());
        let get = |n: &str| back.iter().find(|x| x.0 == n).map(|x| x.4.clone());
        assert_eq!(get(&deep), Some(b"a".to_vec()));
        assert_eq!(get(&tail), Some(b"b".to_vec()));
    }

    /// A name that is not UTF-8 would be renamed in the image: refused.
    #[cfg(target_os = "linux")]
    #[test]
    fn names_that_are_not_utf8_are_refused() {
        use std::os::unix::ffi::OsStrExt;
        let root = std::env::temp_dir().join(format!("giongimagenames-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("static")).unwrap();
        let name = std::ffi::OsStr::from_bytes(b"caf\xe9.txt");
        std::fs::write(root.join("static").join(name), "x").unwrap();
        let e = collect(&root, "static", &mut Vec::new()).unwrap_err();
        assert_eq!(e.code, Code::IMAGE_ADD);
        assert!(e.what.contains("not UTF-8"), "{}", e.what);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn the_oci_files_reference_each_other() {
        let layer = tar(&[Entry::file("app/svc", 0o755, b"bin".to_vec())], 0).unwrap();
        let meta = Meta {
            name: "svc".into(),
            tag: "1.0.0".into(),
            arch: "amd64",
            binary: "svc".into(),
            port: 3000,
            created: 0,
            labels: BTreeMap::new(),
        };
        let (files, digest) = oci(&layer, &meta);
        let get = |p: &str| {
            files
                .iter()
                .find(|f| f.0 == p)
                .map(|f| f.1.clone())
                .unwrap()
        };
        let index: Value = serde_json::from_slice(&get("index.json")).unwrap();
        assert_eq!(index["manifests"][0]["digest"], digest);
        assert_eq!(
            index["manifests"][0]["annotations"]["io.containerd.image.name"],
            "docker.io/library/svc:1.0.0"
        );
        assert_eq!(qualified("owner/svc:1"), "docker.io/owner/svc:1");
        assert_eq!(qualified("ghcr.io/o/svc:1"), "ghcr.io/o/svc:1");
        assert_eq!(qualified("localhost:5000/svc:1"), "localhost:5000/svc:1");
        let manifest_bytes = get(&format!("blobs/sha256/{}", &digest[7..]));
        assert_eq!(format!("sha256:{}", sha256::hex(&manifest_bytes)), digest);
        let manifest: Value = serde_json::from_slice(&manifest_bytes).unwrap();
        let cfg_digest = manifest["config"]["digest"].as_str().unwrap();
        let cfg: Value =
            serde_json::from_slice(&get(&format!("blobs/sha256/{}", &cfg_digest[7..]))).unwrap();
        assert_eq!(cfg["config"]["Entrypoint"][0], "/app/svc");
        assert_eq!(cfg["config"]["User"], "65532:65532");
        assert!(cfg["config"]["ExposedPorts"].get("3000/tcp").is_some());
        assert!(
            cfg["config"]["Env"]
                .as_array()
                .unwrap()
                .iter()
                .any(|e| e == "LISTEN=0.0.0.0:3000"),
            "{}",
            cfg["config"]["Env"]
        );
        assert_eq!(
            cfg["rootfs"]["diff_ids"][0],
            manifest["layers"][0]["digest"]
        );
        assert_eq!(manifest["layers"][0]["size"], layer.len());
        let docker: Value = serde_json::from_slice(&get("manifest.json")).unwrap();
        assert_eq!(docker[0]["RepoTags"][0], "svc:1.0.0");
        // The same input, the same digest.
        assert_eq!(oci(&layer, &meta).1, digest);
    }
}
