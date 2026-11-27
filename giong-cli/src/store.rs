//! The build store: one copy on disk of each dependency build, shared by every
//! project on the machine.
//!
//! Every Rust project keeps its own build of each dependency. Ten services
//! that use the same giong, rustls and serde hold ten copies of each, and
//! cargo never removes one. The store keeps one: after a build, each file a
//! dependency's build wrote is replaced by a hard link to the same bytes in
//! `~/.giong/store/objects` (made from the first project that built it). The
//! projects keep their own `target/`, so nothing changes for cargo: no shared
//! lock to wait on, no binary of one project overwriting another's, and
//! `cargo clean` in one project only drops that project's links.
//!
//! Why this is safe:
//!
//! - **Same bytes only.** A file is linked to an object only when the two are
//!   byte for byte the same (compared, not hashed), and the object belongs to
//!   the same user and cannot be written by others. What cargo reads is what
//!   it built.
//! - **Nothing writes into a shared file.** rustc writes a new library and
//!   its metadata and renames them over the old ones, and so do the linkers
//!   of Linux and macOS for shared libraries and programs: a rebuild gives
//!   the project a file of its own and leaves the object alone
//!   (`rustc_replaces_its_outputs` checks this on each platform CI runs).
//!   MSVC's `link.exe` writes into the file it finds, so on Windows only
//!   libraries and their metadata are shared.
//! - **Cargo still sees its builds as fresh.** Cargo up to 1.100 calls a unit
//!   stale when one of its dependencies' output files is newer than its own,
//!   and a hard link shares one modification time between projects. So every
//!   output of a dependency unit gets the same time in every project:
//!   [`BASE_MTIME`] plus its height in the dependency graph (read from cargo's
//!   own fingerprints), so a unit is always newer than what it depends on and
//!   older than the project's own code. A unit whose build script watches a
//!   file newer than that time keeps its own times, and so does everything
//!   that depends on it. (Newer cargo compares fingerprint timestamps instead
//!   and does not look at these times at all.)
//! - **Only while cargo is out.** A project is handled only while giong holds
//!   cargo's own lock on its build directory; a running build makes giong
//!   skip it, and a build started meanwhile waits a moment for the lock.
//!
//! Only dependencies from a registry or git are shared (they rarely change
//! and are the same across projects); the project's own crates are not.
//! Only what rustc itself writes is shared ([`rustc_writes`]): never a file
//! a build script wrote in `OUT_DIR` (a build script that runs again may
//! write into the file it finds there, which would change every project's
//! copy), never object files or debug information a linker may update.
//! Only cargo's build-dir layout (cargo 1.100 and later) is handled.
//!
//! An object nothing links to any more is removed by [`gc`], which also keeps
//! the builds of the projects giong knows under a size budget.

use std::collections::{HashMap, HashSet};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};

/// The modification time of a shared unit of height 0: 2020-01-01 00:00 UTC.
/// Later than the time cargo gives every file of a crate it downloads
/// (2006-07-24), earlier than any file a person edits today.
pub const BASE_MTIME: u64 = 1_577_836_800;

/// Files smaller than this are left alone: a link saves almost nothing.
const MIN_SHARED: u64 = 16 * 1024;

/// How many different contents one unit's file may have in the store (a
/// crate whose build script writes code with the project's path in it
/// differs per project).
const MAX_VARIANTS: usize = 16;

/// The name of a link while it is put in place.
const TMP_SUFFIX: &str = ".giong-store-tmp";

/// The default size budget for the builds of the projects giong knows.
pub const DEFAULT_BUDGET: u64 = 30 << 30;

/// A project not built for this long may have its whole build removed when
/// the builds are over budget.
pub const IDLE_DAYS: u64 = 14;

/// How often `giong dev` runs [`gc`] on its own.
pub const GC_EVERY: Duration = Duration::from_secs(3600);

/// The store, opened.
pub struct Store {
    pub root: PathBuf,
}

/// Why the store cannot be used.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Unusable {
    /// `GIONG_STORE=off`.
    Off,
    /// No home directory to put it in.
    NoHome,
    /// Built by a compiler without file locks (Rust 1.89).
    NoFileLocks,
    /// The store directory can be written by other users.
    NotPrivate(PathBuf),
    Io(String),
}

impl Unusable {
    pub fn why(&self) -> String {
        match self {
            Unusable::Off => "GIONG_STORE=off turns the build store off".into(),
            Unusable::NoHome => "neither HOME nor USERPROFILE is set".into(),
            Unusable::NoFileLocks => {
                "this giong was built with a Rust older than 1.89, which has no file locks".into()
            }
            Unusable::NotPrivate(p) => format!(
                "{} can be written by other users, so its files cannot be trusted",
                p.display()
            ),
            Unusable::Io(e) => e.clone(),
        }
    }

    pub fn fix(&self) -> &'static str {
        match self {
            Unusable::Off => "unset GIONG_STORE to share builds again",
            Unusable::NoHome => "set HOME (or GIONG_STORE_DIR) to a directory of yours",
            Unusable::NoFileLocks => {
                "rustup update stable, then cargo install giong-cli (or giong upgrade --self)"
            }
            Unusable::NotPrivate(_) => {
                "chmod go-w the directory, or set GIONG_STORE_DIR to a directory only you can write"
            }
            Unusable::Io(_) => "check that the store directory (GIONG_STORE_DIR) can be written",
        }
    }
}

/// The store directory: `GIONG_STORE_DIR`, else `~/.giong/store`.
pub fn default_root() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("GIONG_STORE_DIR").filter(|d| !d.is_empty()) {
        return Some(PathBuf::from(dir));
    }
    crate::versions::giong_home().map(|h| h.join("store"))
}

/// Whether `GIONG_STORE` turns the store off.
pub fn turned_off() -> bool {
    std::env::var("GIONG_STORE")
        .map(|v| matches!(v.trim(), "off" | "0" | "false" | "no"))
        .unwrap_or(false)
}

impl Store {
    /// The store of this user, made if missing.
    pub fn open() -> Result<Store, Unusable> {
        if turned_off() {
            return Err(Unusable::Off);
        }
        let root = default_root().ok_or(Unusable::NoHome)?;
        Store::open_at(&root)
    }

    /// The store at `root`, made if missing.
    pub fn open_at(root: &Path) -> Result<Store, Unusable> {
        if !lock::AVAILABLE {
            return Err(Unusable::NoFileLocks);
        }
        let objects = root.join("objects");
        make_private_dirs(&objects)
            .map_err(|e| Unusable::Io(format!("{} cannot be made: {e}", objects.display())))?;
        for dir in [root, objects.as_path()] {
            let meta = fs::symlink_metadata(dir)
                .map_err(|e| Unusable::Io(format!("{}: {e}", dir.display())))?;
            if !meta.is_dir() || !private(&meta) {
                return Err(Unusable::NotPrivate(dir.to_path_buf()));
            }
        }
        Ok(Store {
            root: root.to_path_buf(),
        })
    }

    pub fn objects(&self) -> PathBuf {
        self.root.join("objects")
    }

    fn lock_path(&self) -> PathBuf {
        self.root.join("lock")
    }

    fn registry_path(&self) -> PathBuf {
        self.root.join("projects.json")
    }
}

/// Makes `dir` and its missing parents, the new ones private to this user.
fn make_private_dirs(dir: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)
    }
    #[cfg(not(unix))]
    {
        fs::create_dir_all(dir)
    }
}

/// No one but the owner may write it.
#[cfg(unix)]
fn private(meta: &fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;
    meta.mode() & 0o022 == 0
}

#[cfg(not(unix))]
fn private(_meta: &fs::Metadata) -> bool {
    // A user's profile folder is theirs alone by default.
    true
}

/// An object may stand in for `own` (a file this user's build wrote): same
/// owner, and no one else can write it.
#[cfg(unix)]
fn trusted(object: &fs::Metadata, own: &fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;
    object.uid() == own.uid() && private(object)
}

#[cfg(not(unix))]
fn trusted(_object: &fs::Metadata, _own: &fs::Metadata) -> bool {
    true
}

/// The file locks cargo takes on its build directory (`flock` on Unix,
/// `LockFileEx` on Windows): the same calls, so each side sees the other's.
/// std has them from Rust 1.89; build.rs sets `giong_file_lock` only then.
#[allow(clippy::incompatible_msrv)]
mod lock {
    use std::fs::File;
    use std::io;
    use std::path::Path;

    #[cfg(giong_file_lock)]
    pub const AVAILABLE: bool = true;
    #[cfg(not(giong_file_lock))]
    pub const AVAILABLE: bool = false;

    /// An exclusive lock on `path` (made if missing when `create`), or
    /// `None` when someone else holds a lock on it.
    pub fn try_exclusive(path: &Path, create: bool) -> io::Result<Option<File>> {
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(create)
            .truncate(false)
            .open(path)?;
        #[cfg(giong_file_lock)]
        {
            match file.try_lock() {
                Ok(()) => Ok(Some(file)),
                Err(std::fs::TryLockError::WouldBlock) => Ok(None),
                Err(std::fs::TryLockError::Error(e)) => Err(e),
            }
        }
        #[cfg(not(giong_file_lock))]
        {
            let _ = file;
            Err(io::Error::new(io::ErrorKind::Unsupported, "no file locks"))
        }
    }

    /// A shared lock on `path`, waited for.
    pub fn shared(path: &Path) -> io::Result<File> {
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)?;
        #[cfg(giong_file_lock)]
        file.lock_shared()?;
        Ok(file)
    }

    /// An exclusive lock on `path`, waited for.
    pub fn exclusive(path: &Path) -> io::Result<File> {
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)?;
        #[cfg(giong_file_lock)]
        file.lock()?;
        Ok(file)
    }
}

/// What the file system says about a file: its identity and link count.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FileId {
    pub dev: u64,
    pub ino: u64,
    pub links: u64,
}

#[cfg(unix)]
pub fn file_id(path: &Path, meta: &fs::Metadata) -> Option<FileId> {
    let _ = path;
    use std::os::unix::fs::MetadataExt;
    Some(FileId {
        dev: meta.dev(),
        ino: meta.ino(),
        links: meta.nlink(),
    })
}

#[cfg(windows)]
pub fn file_id(path: &Path, _meta: &fs::Metadata) -> Option<FileId> {
    crate::winproc::file_id(path)
}

#[cfg(not(any(unix, windows)))]
pub fn file_id(_path: &Path, _meta: &fs::Metadata) -> Option<FileId> {
    None
}

fn links(path: &Path, meta: &fs::Metadata) -> Option<u64> {
    file_id(path, meta).map(|i| i.links)
}

/// Seconds since the Unix epoch, when the time is a whole second.
fn whole_seconds(meta: &fs::Metadata) -> Option<u64> {
    let d = meta.modified().ok()?.duration_since(UNIX_EPOCH).ok()?;
    (d.subsec_nanos() == 0).then_some(d.as_secs())
}

fn set_mtime(path: &Path, secs: u64) -> io::Result<()> {
    let f = OpenOptions::new().write(true).open(path)?;
    f.set_modified(UNIX_EPOCH + Duration::from_secs(secs))
}

/// One unit of a cargo build directory (build-dir layout):
/// `build/<package>/<hash>/` with `fingerprint/` and `out/` or `run/`.
#[derive(Debug)]
struct Unit {
    dir: PathBuf,
    package: String,
    hash: String,
    /// Running a build script (its output is `run/stdout`).
    run: bool,
    /// The unit's fingerprint, as cargo writes it (16 hex digits).
    fingerprint: String,
    /// The fingerprints of what it depends on.
    deps: Vec<String>,
    /// cargo's `local` entries (how it checks the unit's own inputs).
    local: Vec<Value>,
    /// The files cargo compares times of.
    outputs: Vec<PathBuf>,
}

/// A real directory (not a link): giong never follows a link in a build
/// directory a cloned repository may have committed.
fn real_dir(p: &Path) -> bool {
    fs::symlink_metadata(p).is_ok_and(|m| m.is_dir())
}

fn entries(dir: &Path) -> Vec<fs::DirEntry> {
    let mut v: Vec<fs::DirEntry> = fs::read_dir(dir).into_iter().flatten().flatten().collect();
    v.sort_by_key(|e| e.file_name());
    v
}

/// Whether `profile` is a profile directory in cargo's build-dir layout.
pub fn is_new_layout(profile: &Path) -> bool {
    profile.join(".cargo-build-lock").is_file() && real_dir(&profile.join("build"))
}

fn scan(profile: &Path) -> Vec<Unit> {
    let mut out = Vec::new();
    let build = profile.join("build");
    if !real_dir(&build) {
        return out;
    }
    for pkg in entries(&build) {
        let pkg_dir = pkg.path();
        if !real_dir(&pkg_dir) {
            continue;
        }
        for u in entries(&pkg_dir) {
            let dir = u.path();
            let hash = u.file_name().to_string_lossy().into_owned();
            let is_hash = hash.len() == 16 && hash.bytes().all(|b| b.is_ascii_hexdigit());
            if !is_hash || !real_dir(&dir) || !real_dir(&dir.join("fingerprint")) {
                continue;
            }
            if let Some(unit) = read_unit(&dir, &pkg.file_name().to_string_lossy(), &hash) {
                out.push(unit);
            }
        }
    }
    out
}

fn read_unit(dir: &Path, package: &str, hash: &str) -> Option<Unit> {
    let fp_dir = dir.join("fingerprint");
    let jsons: Vec<PathBuf> = entries(&fp_dir)
        .into_iter()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "json"))
        .collect();
    let [json_path] = jsons.as_slice() else {
        return None;
    };
    let stem = json_path.file_stem()?.to_str()?.to_string();
    let v: Value = serde_json::from_str(&fs::read_to_string(json_path).ok()?).ok()?;
    let fingerprint = fs::read_to_string(fp_dir.join(&stem))
        .ok()?
        .trim()
        .to_string();
    let deps = v["deps"]
        .as_array()?
        .iter()
        .map(|d| d.get(3).and_then(Value::as_u64).map(to_hex))
        .collect::<Option<Vec<String>>>()?;
    let local = v["local"].as_array().cloned().unwrap_or_default();
    let run = stem.starts_with("run-");
    let outputs = if run {
        vec![dir.join("run").join("stdout")]
    } else {
        let out = dir.join("out");
        if !real_dir(&out) {
            return None;
        }
        entries(&out)
            .into_iter()
            .map(|e| e.path())
            .filter(|p| {
                let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
                !name.ends_with(".d") && !name.ends_with(TMP_SUFFIX)
            })
            .collect()
    };
    Some(Unit {
        dir: dir.to_path_buf(),
        package: package.to_string(),
        hash: hash.to_string(),
        run,
        fingerprint,
        deps,
        local,
        outputs,
    })
}

/// A fingerprint as cargo writes it in the unit's fingerprint file.
fn to_hex(n: u64) -> String {
    n.to_le_bytes().iter().map(|b| format!("{b:02x}")).collect()
}

/// The first source file a dep-info file names (`target: source ...`).
fn first_source(dep_info: &str) -> Option<PathBuf> {
    let line = dep_info.lines().next()?;
    let (_, rest) = line.split_once(": ")?;
    let mut path = String::new();
    let mut chars = rest.trim_start().chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\\' if chars.peek() == Some(&' ') => {
                path.push(' ');
                chars.next();
            }
            ' ' => break,
            c => path.push(c),
        }
    }
    (!path.is_empty()).then(|| PathBuf::from(path))
}

/// The package directory of a build script: the nearest directory above its
/// first source that holds a Cargo.toml.
fn package_root(build_script: &Unit) -> Option<PathBuf> {
    let out = build_script.dir.join("out");
    let dep_info = entries(&out)
        .into_iter()
        .map(|e| e.path())
        .find(|p| p.extension().is_some_and(|x| x == "d"))?;
    let source = first_source(&fs::read_to_string(dep_info).ok()?)?;
    source
        .ancestors()
        .skip(1)
        .find(|d| d.join("Cargo.toml").is_file())
        .map(Path::to_path_buf)
}

/// Whether the times cargo checks for this unit's own inputs all stay
/// right when its outputs get the shared time `mtime`.
fn inputs_older(unit: &Unit, by_fp: &HashMap<&str, usize>, units: &[Unit], mtime: u64) -> bool {
    for local in &unit.local {
        if local.get("Precalculated").is_some()
            || local.get("CheckDepInfo").is_some()
            || local.get("RerunIfEnvChanged").is_some()
        {
            // Precalculated: a version or git revision; CheckDepInfo: the
            // sources against the dep-info file, which is not touched;
            // environment: no time at all.
            continue;
        }
        let Some(watched) = local.get("RerunIfChanged") else {
            return false; // a kind this code does not know
        };
        let paths = watched["paths"].as_array().cloned().unwrap_or_default();
        if paths.is_empty() {
            continue;
        }
        // The paths are relative to the package; it is found through the
        // build script's own dep-info.
        let root = unit
            .deps
            .iter()
            .filter_map(|d| by_fp.get(d.as_str()).map(|&i| &units[i]))
            .filter(|d| !d.run)
            .find_map(package_root);
        let Some(root) = root else {
            return false;
        };
        for p in paths {
            let Some(p) = p.as_str() else { return false };
            let newer = fs::metadata(root.join(p))
                .ok()
                .and_then(|m| m.modified().ok())
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .map_or(true, |d| d.as_secs() >= mtime);
            if newer {
                return false;
            }
        }
    }
    true
}

/// The height of every unit that can be shared (`None`: it keeps its own
/// times): from a registry or git, every dependency shareable too, and
/// inputs older than the shared time.
fn heights(units: &[Unit], local_packages: &HashSet<String>) -> Vec<Option<u64>> {
    let by_fp: HashMap<&str, usize> = units
        .iter()
        .enumerate()
        .map(|(i, u)| (u.fingerprint.as_str(), i))
        .collect();
    let mut memo: Vec<Option<Option<u64>>> = vec![None; units.len()];
    let mut visiting = vec![false; units.len()];
    fn visit(
        i: usize,
        units: &[Unit],
        by_fp: &HashMap<&str, usize>,
        local_packages: &HashSet<String>,
        memo: &mut Vec<Option<Option<u64>>>,
        visiting: &mut Vec<bool>,
    ) -> Option<u64> {
        if let Some(h) = memo[i] {
            return h;
        }
        if visiting[i] {
            return None;
        }
        visiting[i] = true;
        let u = &units[i];
        let mut h = if local_packages.contains(&u.package) {
            None
        } else {
            Some(0)
        };
        for d in &u.deps {
            if h.is_none() {
                break;
            }
            h = match by_fp.get(d.as_str()) {
                Some(&j) => visit(j, units, by_fp, local_packages, memo, visiting)
                    .and_then(|dh| h.map(|cur| cur.max(dh + 1))),
                None => None,
            };
        }
        if let Some(height) = h {
            if !inputs_older(u, by_fp, units, BASE_MTIME + height) {
                h = None;
            }
        }
        visiting[i] = false;
        memo[i] = Some(h);
        h
    }
    (0..units.len())
        .map(|i| visit(i, units, &by_fp, local_packages, &mut memo, &mut visiting))
        .collect()
}

/// What one pass over a profile directory did.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Pass {
    pub units: usize,
    /// Units whose files are shared (or could be, once another project has them).
    pub shared_units: usize,
    /// Files replaced by a link to an object another project made.
    pub linked_files: u64,
    pub linked_bytes: u64,
    /// Objects this pass added (links to this project's files: no disk).
    pub new_objects: u64,
    /// Stopped early because a build wanted the directory.
    pub cancelled: bool,
}

/// Why a profile directory was not handled.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Skip {
    /// A cargo build holds the directory.
    Busy,
    /// Not cargo's build-dir layout (cargo 1.99 or older wrote it).
    OldLayout,
    /// A link cannot be made (the store is on another drive, or the file
    /// system has no hard links).
    CannotLink(String),
    Io(String),
}

impl Skip {
    pub fn why(&self) -> String {
        match self {
            Skip::Busy => "a cargo build is using it".into(),
            Skip::OldLayout => "cargo 1.99 or older built it (the store needs cargo 1.100)".into(),
            Skip::CannotLink(e) => format!("no hard link can be made to the store: {e}"),
            Skip::Io(e) => e.clone(),
        }
    }
}

/// Holds cargo's locks on a profile directory.
struct CargoLocks {
    _files: Vec<File>,
}

fn lock_profile(profile: &Path) -> Result<CargoLocks, Skip> {
    let mut files = Vec::new();
    // Cargo 1.100 locks `.cargo-build-lock`; older cargo `.cargo-lock`.
    for name in [".cargo-build-lock", ".cargo-lock"] {
        let path = profile.join(name);
        if !path.is_file() {
            continue;
        }
        match lock::try_exclusive(&path, false) {
            Ok(Some(f)) => files.push(f),
            Ok(None) => return Err(Skip::Busy),
            Err(e) => return Err(Skip::Io(format!("{}: {e}", path.display()))),
        }
    }
    if files.is_empty() {
        return Err(Skip::OldLayout);
    }
    Ok(CargoLocks { _files: files })
}

/// The packages of a workspace that are not from a registry or git (its
/// members and path dependencies), from its Cargo.lock.
pub fn local_packages(project_root: &Path) -> Option<HashSet<String>> {
    let text = fs::read_to_string(project_root.join("Cargo.lock")).ok()?;
    Some(
        crate::lockfile::parse_lock(&text)
            .into_iter()
            .filter(|p| p.source == crate::lockfile::Source::Path)
            .map(|p| p.name)
            .collect(),
    )
}

/// Shares the dependency builds of one profile directory (`target/debug`,
/// or the build directory's) with the store. `cancel` stops it between
/// files; what was done stays consistent.
pub fn share(
    store: &Store,
    profile: &Path,
    local_packages: &HashSet<String>,
    cancel: &AtomicBool,
) -> Result<Pass, Skip> {
    if !is_new_layout(profile) {
        return Err(Skip::OldLayout);
    }
    let _cargo = lock_profile(profile)?;
    let _store_lock = lock::shared(&store.lock_path()).map_err(|e| Skip::Io(e.to_string()))?;
    let units = scan(profile);
    let mut pass = Pass {
        units: units.len(),
        ..Pass::default()
    };
    // Links left by a pass that was stopped half way.
    for u in &units {
        for e in entries(&u.dir.join("out")) {
            if e.file_name().to_string_lossy().ends_with(TMP_SUFFIX) {
                let _ = fs::remove_file(e.path());
            }
        }
    }
    let mut height = heights(&units, local_packages);
    let by_fp: HashMap<&str, usize> = units
        .iter()
        .enumerate()
        .map(|(i, u)| (u.fingerprint.as_str(), i))
        .collect();
    // A file shared already carries its time for every project that links
    // it; a unit whose file has another time keeps its own (and so do its
    // dependents).
    loop {
        let mut changed = false;
        for (i, u) in units.iter().enumerate() {
            let Some(h) = height[i] else { continue };
            let mtime = BASE_MTIME + h;
            let outputs_ok = !u.outputs.is_empty()
                && u.outputs.iter().all(|o| match fs::symlink_metadata(o) {
                    Ok(m) if m.is_file() => {
                        links(o, &m) == Some(1) || whole_seconds(&m) == Some(mtime)
                    }
                    _ => false,
                });
            let deps_ok = u
                .deps
                .iter()
                .all(|d| by_fp.get(d.as_str()).is_some_and(|&j| height[j].is_some()));
            if !outputs_ok || !deps_ok {
                height[i] = None;
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }
    // Times first, lowest units first: a unit whose time cannot be set
    // leaves its dependents as they are.
    let mut order: Vec<usize> = (0..units.len()).filter(|&i| height[i].is_some()).collect();
    order.sort_by_key(|&i| height[i]);
    let mut timed = vec![false; units.len()];
    for &i in &order {
        let u = &units[i];
        let mtime = BASE_MTIME + height[i].expect("filtered");
        let deps_timed = u
            .deps
            .iter()
            .all(|d| by_fp.get(d.as_str()).is_some_and(|&j| timed[j]));
        if !deps_timed {
            continue;
        }
        let mut ok = true;
        for o in &u.outputs {
            let Ok(m) = fs::symlink_metadata(o) else {
                ok = false;
                break;
            };
            if whole_seconds(&m) != Some(mtime) && set_mtime(o, mtime).is_err() {
                ok = false;
                break;
            }
        }
        timed[i] = ok;
    }
    let objects = store.objects();
    for &i in &order {
        if !timed[i] {
            continue;
        }
        pass.shared_units += 1;
        let u = &units[i];
        if u.run {
            continue; // a build script's output is small and per project
        }
        let mtime = BASE_MTIME + height[i].expect("filtered");
        for o in &u.outputs {
            if cancel.load(Ordering::Relaxed) {
                pass.cancelled = true;
                return Ok(pass);
            }
            let Ok(own) = fs::symlink_metadata(o) else {
                continue;
            };
            if own.len() < MIN_SHARED || links(o, &own) != Some(1) || !rustc_writes(o) {
                continue;
            }
            let bucket = objects.join(format!("{}-{}", u.package, u.hash));
            make_private_dirs(&bucket).map_err(|e| Skip::Io(e.to_string()))?;
            match place(o, &own, &bucket, mtime)? {
                Placed::Linked => {
                    pass.linked_files += 1;
                    pass.linked_bytes += own.len();
                }
                Placed::NewObject => pass.new_objects += 1,
                Placed::Nothing => {}
            }
        }
    }
    Ok(pass)
}

/// The files a build writes by renaming a new file over the old one, so a
/// rebuild never writes into a shared object. rustc writes libraries and
/// their metadata that way on every platform. Proc-macro libraries and
/// build-script programs come from the platform's linker: the linkers of
/// Linux and macOS replace the file too, but MSVC's `link.exe` writes into
/// the one it finds, so on Windows they stay each project's own.
/// `rustc_replaces_its_outputs` checks every kind this shares.
fn rustc_writes(path: &Path) -> bool {
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    if name.ends_with(".rlib") || name.ends_with(".rmeta") {
        return true;
    }
    !cfg!(windows)
        && ([".so", ".dylib"].iter().any(|ext| name.ends_with(ext)) || name == "build_script_build")
}

enum Placed {
    Linked,
    NewObject,
    Nothing,
}

/// Links `file` to an object with the same bytes in `bucket`, or makes it
/// one.
fn place(file: &Path, own: &fs::Metadata, bucket: &Path, mtime: u64) -> Result<Placed, Skip> {
    let name = file
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| Skip::Io(format!("{} has no name", file.display())))?;
    for k in 0..MAX_VARIANTS {
        let candidate = if k == 0 {
            bucket.join(name)
        } else {
            bucket.join(format!("{k}.{name}"))
        };
        match fs::symlink_metadata(&candidate) {
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                return match fs::hard_link(file, &candidate) {
                    Ok(()) => Ok(Placed::NewObject),
                    // Another project made it this instant: compare with it.
                    Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
                    Err(e) => Err(Skip::CannotLink(e.to_string())),
                };
            }
            Err(e) => return Err(Skip::Io(e.to_string())),
            Ok(m) => {
                let same = m.is_file()
                    && m.len() == own.len()
                    && whole_seconds(&m) == Some(mtime)
                    && trusted(&m, own)
                    && same_bytes(&candidate, file).unwrap_or(false);
                if !same {
                    continue;
                }
                let tmp = file.with_file_name(format!("{name}{TMP_SUFFIX}"));
                let _ = fs::remove_file(&tmp);
                if let Err(e) = fs::hard_link(&candidate, &tmp) {
                    return Err(Skip::CannotLink(e.to_string()));
                }
                if let Err(e) = fs::rename(&tmp, file) {
                    let _ = fs::remove_file(&tmp);
                    return Err(Skip::Io(e.to_string()));
                }
                return Ok(Placed::Linked);
            }
        }
    }
    Ok(Placed::Nothing)
}

fn same_bytes(a: &Path, b: &Path) -> io::Result<bool> {
    let mut fa = File::open(a)?;
    let mut fb = File::open(b)?;
    let mut ba = vec![0u8; 1 << 20];
    let mut bb = vec![0u8; 1 << 20];
    loop {
        let na = read_full(&mut fa, &mut ba)?;
        let nb = read_full(&mut fb, &mut bb)?;
        if na != nb || ba[..na] != bb[..nb] {
            return Ok(false);
        }
        if na == 0 {
            return Ok(true);
        }
    }
}

fn read_full(f: &mut File, buf: &mut [u8]) -> io::Result<usize> {
    let mut n = 0;
    while n < buf.len() {
        match f.read(&mut buf[n..])? {
            0 => break,
            k => n += k,
        }
    }
    Ok(n)
}

/// The profile directories of a build directory in cargo's build-dir
/// layout (`debug`, `release`, and the same under `<target-triple>/`).
pub fn profiles(build_dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for e in entries(build_dir) {
        let p = e.path();
        if !real_dir(&p) {
            continue;
        }
        if is_new_layout(&p) {
            out.push(p);
        } else {
            for i in entries(&p) {
                if real_dir(&i.path()) && is_new_layout(&i.path()) {
                    out.push(i.path());
                }
            }
        }
    }
    out
}

/// A project whose builds the store knows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Known {
    /// Its build directory (`build.build-dir`, or the target directory).
    pub build_dir: PathBuf,
    pub root: PathBuf,
    /// When giong last saw it built (Unix seconds).
    pub last_built: u64,
}

/// The projects and the time of the last [`gc`].
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Registry {
    pub projects: Vec<Known>,
    pub last_gc: u64,
}

impl Registry {
    fn parse(text: &str) -> Registry {
        let v: Value = serde_json::from_str(text).unwrap_or(Value::Null);
        let projects = v["projects"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|p| {
                Some(Known {
                    build_dir: PathBuf::from(p["build_dir"].as_str()?),
                    root: PathBuf::from(p["root"].as_str()?),
                    last_built: p["last_built"].as_u64().unwrap_or(0),
                })
            })
            .collect();
        Registry {
            projects,
            last_gc: v["last_gc"].as_u64().unwrap_or(0),
        }
    }

    fn to_json(&self) -> Value {
        json!({
            "projects": self.projects.iter().map(|p| json!({
                "build_dir": p.build_dir.display().to_string(),
                "root": p.root.display().to_string(),
                "last_built": p.last_built,
            })).collect::<Vec<_>>(),
            "last_gc": self.last_gc,
        })
    }
}

pub fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// The registry, read.
pub fn registry(store: &Store) -> Registry {
    fs::read_to_string(store.registry_path())
        .map(|t| Registry::parse(&t))
        .unwrap_or_default()
}

/// Changes the registry under the store's lock.
fn update_registry(store: &Store, f: impl FnOnce(&mut Registry)) -> io::Result<()> {
    let _lock = lock::exclusive(&store.lock_path())?;
    let mut reg = registry(store);
    f(&mut reg);
    let tmp = store.root.join("projects.json.tmp");
    fs::write(&tmp, serde_json::to_vec_pretty(&reg.to_json())?)?;
    fs::rename(&tmp, store.registry_path())
}

/// Records that `build_dir` (of the project at `root`) was built now.
pub fn note_built(store: &Store, build_dir: &Path, root: &Path) -> io::Result<()> {
    let build_dir = fs::canonicalize(build_dir).unwrap_or_else(|_| build_dir.to_path_buf());
    let root = fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    let t = now();
    update_registry(store, |reg| {
        match reg.projects.iter_mut().find(|p| p.build_dir == build_dir) {
            Some(p) => {
                p.last_built = t;
                p.root = root;
            }
            None => reg.projects.push(Known {
                build_dir,
                root,
                last_built: t,
            }),
        }
    })
}

/// What the store holds.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Usage {
    pub objects: u64,
    /// Bytes of the objects (each once).
    pub object_bytes: u64,
    /// Bytes the projects would use without the store.
    pub saved_bytes: u64,
    /// Objects no project links any more.
    pub unused: u64,
    pub unused_bytes: u64,
}

/// Every object file: (path, metadata).
fn objects(store: &Store) -> Vec<(PathBuf, fs::Metadata)> {
    let mut out = Vec::new();
    for bucket in entries(&store.objects()) {
        if !real_dir(&bucket.path()) {
            continue;
        }
        for o in entries(&bucket.path()) {
            if let Ok(m) = fs::symlink_metadata(o.path()) {
                if m.is_file() {
                    out.push((o.path(), m));
                }
            }
        }
    }
    out
}

pub fn usage(store: &Store) -> Usage {
    let mut u = Usage::default();
    for (path, meta) in objects(store) {
        let Some(n) = links(&path, &meta) else {
            continue;
        };
        u.objects += 1;
        u.object_bytes += meta.len();
        if n <= 1 {
            u.unused += 1;
            u.unused_bytes += meta.len();
        } else {
            // The store's link and the first project's are one copy; each
            // other project's link is a copy not on disk.
            u.saved_bytes += meta.len() * (n - 2);
        }
    }
    u
}

/// The bytes on disk under `dirs`, each file once however many links it has.
pub fn disk_used(dirs: &[PathBuf]) -> u64 {
    let mut seen: HashSet<(u64, u64)> = HashSet::new();
    let mut total = 0;
    let mut stack: Vec<PathBuf> = dirs.to_vec();
    while let Some(d) = stack.pop() {
        for e in entries(&d) {
            let Ok(meta) = fs::symlink_metadata(e.path()) else {
                continue;
            };
            if meta.is_dir() {
                stack.push(e.path());
            } else if meta.is_file() {
                let first = match file_id(&e.path(), &meta) {
                    Some(id) if id.links > 1 => seen.insert((id.dev, id.ino)),
                    _ => true,
                };
                if first {
                    total += meta.len();
                }
            }
        }
    }
    total
}

/// When a build directory was last built: the newest unit timestamp cargo
/// wrote in it, or what the registry says.
fn last_built(known: &Known) -> u64 {
    let mut newest = known.last_built;
    for profile in profiles(&known.build_dir) {
        for pkg in entries(&profile.join("build")) {
            for u in entries(&pkg.path()) {
                let t = fs::metadata(u.path().join("fingerprint").join("invoked.timestamp"))
                    .ok()
                    .and_then(|m| m.modified().ok())
                    .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                    .map(|d| d.as_secs())
                    .unwrap_or(0);
                newest = newest.max(t);
            }
        }
    }
    newest
}

/// What [`gc`] did (or, with `dry`, would do).
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Gc {
    pub unused_objects: u64,
    pub unused_bytes: u64,
    /// The projects' builds before, each file once.
    pub used_before: u64,
    pub used_after: u64,
    /// (build directory, bytes, what was removed).
    pub trimmed: Vec<(PathBuf, u64, &'static str)>,
    /// Build directories that no longer exist, dropped from the registry.
    pub forgotten: Vec<PathBuf>,
    pub cancelled: bool,
}

/// Removes the objects no project links any more, then, when the builds of
/// the projects giong knows use more than `budget`, frees them in order:
/// what an older compiler built and incremental data unused for 14 days in
/// every project, then the whole build of projects not built for
/// [`IDLE_DAYS`] days, the longest idle first. Never `keep` (the project
/// giong dev serves) and never a build directory cargo is using.
pub fn gc(
    store: &Store,
    budget: Option<u64>,
    keep: Option<&Path>,
    dry: bool,
    cancel: &AtomicBool,
) -> Gc {
    let mut g = Gc::default();
    let keep = keep.map(|k| fs::canonicalize(k).unwrap_or_else(|_| k.to_path_buf()));
    remove_unused_objects(store, dry, &mut g);
    let reg = registry(store);
    let (live, gone): (Vec<Known>, Vec<Known>) = reg
        .projects
        .into_iter()
        .partition(|p| real_dir(&p.build_dir));
    g.forgotten = gone.into_iter().map(|p| p.build_dir).collect();
    let dirs: Vec<PathBuf> = live.iter().map(|p| p.build_dir.clone()).collect();
    g.used_before = disk_used(&dirs);
    g.used_after = g.used_before;
    if let Some(budget) = budget {
        if g.used_before > budget {
            trim(&live, keep.as_deref(), budget, dry, cancel, &mut g);
            if !dry {
                remove_unused_objects(store, false, &mut g);
                g.used_after = disk_used(&dirs);
            }
        }
    }
    if !dry {
        let forgotten = g.forgotten.clone();
        let _ = update_registry(store, |r| {
            r.projects.retain(|p| !forgotten.contains(&p.build_dir));
            r.last_gc = now();
        });
    }
    g
}

fn remove_unused_objects(store: &Store, dry: bool, g: &mut Gc) {
    let Ok(_lock) = lock::exclusive(&store.lock_path()) else {
        return;
    };
    for (path, meta) in objects(store) {
        if links(&path, &meta) == Some(1) && (dry || fs::remove_file(&path).is_ok()) {
            g.unused_objects += 1;
            g.unused_bytes += meta.len();
        }
    }
    if !dry {
        for bucket in entries(&store.objects()) {
            let _ = fs::remove_dir(bucket.path()); // only when empty
        }
    }
}

fn trim(
    live: &[Known],
    keep: Option<&Path>,
    budget: u64,
    dry: bool,
    cancel: &AtomicBool,
    g: &mut Gc,
) {
    let dirs: Vec<PathBuf> = live.iter().map(|p| p.build_dir.clone()).collect();
    let mut used = g.used_before;
    let candidates: Vec<&Known> = live
        .iter()
        .filter(|p| keep != Some(p.build_dir.as_path()))
        .collect();
    // 1. What no build of the current toolchain uses.
    let mut probes: HashMap<String, Option<u64>> = HashMap::new();
    for p in &candidates {
        if used <= budget || cancel.load(Ordering::Relaxed) {
            break;
        }
        let key = crate::host::output_in(&p.root, "rustc", &["-vV"]).unwrap_or_default();
        let rustc = match probes.get(&key) {
            Some(h) => *h,
            None => {
                let h = crate::cmd::clean::current_rustc_hash(&p.root)
                    .ok()
                    .flatten();
                probes.insert(key, h);
                h
            }
        };
        let plan = crate::cmd::clean::plan(&p.build_dir, rustc, None);
        if plan.paths.is_empty() {
            continue;
        }
        let freed = remove_locked(&p.build_dir, &plan.paths, dry);
        if let Some(freed) = freed {
            if freed > 0 {
                g.trimmed.push((p.build_dir.clone(), freed, plan.reason));
            }
            used = if dry {
                used.saturating_sub(freed)
            } else {
                disk_used(&dirs)
            };
        }
    }
    // 2. Whole builds of idle projects, the longest idle first.
    let mut idle: Vec<(u64, &Known)> = candidates
        .iter()
        .map(|p| (last_built(p), *p))
        .filter(|(t, _)| now().saturating_sub(*t) >= IDLE_DAYS * 86_400)
        .collect();
    idle.sort_by_key(|(t, _)| *t);
    for (_, p) in idle {
        if used <= budget || cancel.load(Ordering::Relaxed) {
            break;
        }
        let paths: Vec<PathBuf> = entries(&p.build_dir)
            .into_iter()
            .map(|e| e.path())
            .filter(|e| e.file_name().is_some_and(|n| n != "CACHEDIR.TAG"))
            .collect();
        if let Some(freed) = remove_locked(&p.build_dir, &paths, dry) {
            g.trimmed
                .push((p.build_dir.clone(), freed, "whole build of an idle project"));
            used = if dry {
                used.saturating_sub(freed)
            } else {
                disk_used(&dirs)
            };
        }
    }
    if cancel.load(Ordering::Relaxed) {
        g.cancelled = true;
    }
}

/// Removes `paths` (in `build_dir`) while holding cargo's locks on each of
/// its profiles: `None` when a build is using it. Returns the bytes freed
/// (with `dry`, what would be).
fn remove_locked(build_dir: &Path, paths: &[PathBuf], dry: bool) -> Option<u64> {
    let mut locks = Vec::new();
    for profile in profiles(build_dir) {
        match lock_profile(&profile) {
            Ok(l) => locks.push(l),
            Err(Skip::OldLayout) => {}
            Err(_) => return None,
        }
    }
    let before = disk_used(paths);
    if dry {
        return Some(before);
    }
    for p in paths {
        let is_dir = fs::symlink_metadata(p).is_ok_and(|m| m.is_dir());
        let _ = if is_dir {
            fs::remove_dir_all(p)
        } else {
            fs::remove_file(p)
        };
    }
    let after = disk_used(paths);
    drop(locks);
    Some(before.saturating_sub(after))
}

/// A size such as `30G`, `512M`, `2048` (bytes), or `off`.
pub fn parse_size(text: &str) -> Option<Option<u64>> {
    let t = text.trim().to_ascii_lowercase();
    if matches!(t.as_str(), "off" | "none" | "0") {
        return Some(None);
    }
    let t = t.trim_end_matches("ib").trim_end_matches('b');
    let (num, shift) = match t.chars().last()? {
        'k' => (&t[..t.len() - 1], 10),
        'm' => (&t[..t.len() - 1], 20),
        'g' => (&t[..t.len() - 1], 30),
        't' => (&t[..t.len() - 1], 40),
        _ => (t, 0),
    };
    let n: f64 = num.trim().parse().ok()?;
    if !(n.is_finite() && n > 0.0) {
        return None;
    }
    Some(Some((n * (1u64 << shift) as f64) as u64))
}

/// The budget: `GIONG_STORE_BUDGET`, else [`DEFAULT_BUDGET`]; `None` means
/// no budget.
pub fn budget_from_env() -> Option<u64> {
    match std::env::var("GIONG_STORE_BUDGET") {
        Ok(v) => parse_size(&v).unwrap_or(Some(DEFAULT_BUDGET)),
        Err(_) => Some(DEFAULT_BUDGET),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("giong-store-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn fingerprints_are_written_as_cargo_writes_them() {
        // From a real build: deps entry 12366807637949887492 is the unit
        // whose fingerprint file holds 0434974906ba9fab.
        assert_eq!(to_hex(12366807637949887492), "0434974906ba9fab");
    }

    #[test]
    fn the_first_source_of_a_dep_info_file() {
        let unix =
            "/t/out/build_script_build: /reg/serde-1.0.0/build.rs\n\n/reg/serde-1.0.0/build.rs:\n";
        assert_eq!(
            first_source(unix),
            Some(PathBuf::from("/reg/serde-1.0.0/build.rs"))
        );
        let windows = "C:\\t\\out\\build_script_build.exe: C:\\reg\\a\\ b-1.0.0\\build.rs\n";
        assert_eq!(
            first_source(windows),
            Some(PathBuf::from("C:\\reg\\a b-1.0.0\\build.rs"))
        );
        assert_eq!(first_source("nothing here"), None);
    }

    #[test]
    fn sizes_are_read() {
        assert_eq!(parse_size("30G"), Some(Some(30 << 30)));
        assert_eq!(parse_size("512mb"), Some(Some(512 << 20)));
        assert_eq!(parse_size("1.5GiB"), Some(Some(3 << 29)));
        assert_eq!(parse_size("2048"), Some(Some(2048)));
        assert_eq!(parse_size("off"), Some(None));
        assert_eq!(parse_size("lots"), None);
        assert_eq!(parse_size("-1G"), None);
    }

    /// Writes a unit the way cargo 1.100 does.
    #[allow(clippy::too_many_arguments)]
    fn unit(
        profile: &Path,
        package: &str,
        hash: &str,
        stem: &str,
        fp: u64,
        deps: &[u64],
        local: Value,
        outputs: &[(&str, &[u8])],
    ) -> PathBuf {
        let dir = profile.join("build").join(package).join(hash);
        fs::create_dir_all(dir.join("fingerprint")).unwrap();
        let deps: Vec<Value> = deps
            .iter()
            .map(|d| json!([1u64, "dep", false, *d]))
            .collect();
        fs::write(
            dir.join("fingerprint").join(format!("{stem}.json")),
            json!({"rustc": 1, "deps": deps, "local": local}).to_string(),
        )
        .unwrap();
        fs::write(dir.join("fingerprint").join(stem), to_hex(fp)).unwrap();
        fs::write(dir.join("fingerprint").join("invoked.timestamp"), "").unwrap();
        let out = if stem.starts_with("run-") {
            dir.join("run")
        } else {
            dir.join("out")
        };
        fs::create_dir_all(&out).unwrap();
        for (name, body) in outputs {
            fs::write(out.join(name), body).unwrap();
        }
        dir
    }

    fn profile(root: &Path) -> PathBuf {
        let p = root.join("target").join("debug");
        fs::create_dir_all(p.join("build")).unwrap();
        fs::write(p.join(".cargo-build-lock"), "").unwrap();
        p
    }

    fn mtime(p: &Path) -> u64 {
        fs::metadata(p)
            .unwrap()
            .modified()
            .unwrap()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs()
    }

    /// Two projects with the same dependency (`base`, and `top` on it) and
    /// their own crate (`app`).
    fn two_projects(root: &Path) -> (PathBuf, PathBuf) {
        let big = vec![7u8; 40 * 1024];
        let mut out = Vec::new();
        for name in ["one", "two"] {
            let p = profile(&root.join(name));
            let pre = json!([{"Precalculated": "1.0.0"}]);
            unit(
                &p,
                "base",
                "00000000000000a1",
                "lib-base",
                1,
                &[],
                pre.clone(),
                &[
                    ("libbase-00000000000000a1.rlib", big.as_slice()),
                    ("libbase-00000000000000a1.rmeta", b"m".as_slice()),
                    ("base-00000000000000a1.d", b"d".as_slice()),
                ],
            );
            unit(
                &p,
                "top",
                "00000000000000b2",
                "lib-top",
                2,
                &[1],
                pre.clone(),
                &[
                    ("libtop-00000000000000b2.rlib", &big[..30 * 1024]),
                    ("libtop-00000000000000b2.rmeta", b"m".as_slice()),
                ],
            );
            let own = format!("app {name}");
            unit(
                &p,
                "app",
                "00000000000000c3",
                "bin-app",
                3,
                &[2],
                json!([{"CheckDepInfo": {"dep_info": "x", "fingerprint": "mtime"}}]),
                &[("app-00000000000000c3", own.as_bytes())],
            );
            out.push(p);
        }
        (out[0].clone(), out[1].clone())
    }

    #[test]
    fn a_second_project_links_to_the_first_ones_build() {
        if !lock::AVAILABLE {
            return;
        }
        let t = temp("share");
        let store = Store::open_at(&t.join("store")).unwrap();
        let (one, two) = two_projects(&t);
        let local: HashSet<String> = ["app".to_string()].into();
        let none = AtomicBool::new(false);
        let first = share(&store, &one, &local, &none).unwrap();
        assert_eq!(first.units, 3);
        assert_eq!(first.shared_units, 2, "base and top, not the project's app");
        assert_eq!(first.linked_files, 0);
        assert_eq!(
            first.new_objects, 2,
            "the two rlibs; the rmeta files are small"
        );
        let second = share(&store, &two, &local, &none).unwrap();
        assert_eq!(second.linked_files, 2);
        assert_eq!(second.linked_bytes, 70 * 1024);
        let rlib =
            |p: &Path| p.join("build/base/00000000000000a1/out/libbase-00000000000000a1.rlib");
        let (a, b) = (
            fs::metadata(rlib(&one)).unwrap(),
            fs::metadata(rlib(&two)).unwrap(),
        );
        assert_eq!(
            file_id(&rlib(&one), &a).map(|i| (i.dev, i.ino)),
            file_id(&rlib(&two), &b).map(|i| (i.dev, i.ino))
        );
        assert_eq!(links(&rlib(&one), &a), Some(3));
        // Heights: base 0, top 1, both below the project's own crate.
        assert_eq!(mtime(&rlib(&two)), BASE_MTIME);
        assert_eq!(
            mtime(&two.join("build/top/00000000000000b2/out/libtop-00000000000000b2.rmeta")),
            BASE_MTIME + 1
        );
        assert!(
            mtime(&two.join("build/app/00000000000000c3/out/app-00000000000000c3"))
                > BASE_MTIME + 1
        );
        // A second pass changes nothing.
        let again = share(&store, &two, &local, &none).unwrap();
        assert_eq!((again.linked_files, again.new_objects), (0, 0));
        let u = usage(&store);
        assert_eq!(u.objects, 2);
        assert_eq!(u.saved_bytes, 70 * 1024);
        fs::remove_dir_all(&t).unwrap();
    }

    #[test]
    fn different_bytes_are_never_linked() {
        if !lock::AVAILABLE {
            return;
        }
        let t = temp("differ");
        let store = Store::open_at(&t.join("store")).unwrap();
        let (one, two) = two_projects(&t);
        let rlib = two.join("build/base/00000000000000a1/out/libbase-00000000000000a1.rlib");
        let mut body = fs::read(&rlib).unwrap();
        body[100] ^= 1;
        fs::write(&rlib, &body).unwrap();
        let local: HashSet<String> = ["app".to_string()].into();
        let none = AtomicBool::new(false);
        share(&store, &one, &local, &none).unwrap();
        let second = share(&store, &two, &local, &none).unwrap();
        assert_eq!(second.linked_files, 1, "only top's rlib is the same");
        assert_eq!(fs::read(&rlib).unwrap(), body);
        assert!(store
            .objects()
            .join("base-00000000000000a1/1.libbase-00000000000000a1.rlib")
            .is_file());
        fs::remove_dir_all(&t).unwrap();
    }

    #[test]
    fn a_build_script_watching_a_new_file_keeps_its_units_times() {
        if !lock::AVAILABLE {
            return;
        }
        let t = temp("watch");
        let store = Store::open_at(&t.join("store")).unwrap();
        let p = profile(&t.join("proj"));
        let pkg = t.join("registry/sys-1.0.0");
        fs::create_dir_all(&pkg).unwrap();
        fs::write(pkg.join("Cargo.toml"), "").unwrap();
        fs::write(pkg.join("build.rs"), "").unwrap(); // written now: newer than BASE_MTIME
        let big = vec![1u8; 20 * 1024];
        let script = unit(
            &p,
            "sys",
            "00000000000000d1",
            "build-script-build-script-build",
            10,
            &[],
            json!([{"Precalculated": "1.0.0"}]),
            &[("build_script_build", big.as_slice())],
        );
        fs::write(
            script.join("out/build_script_build.d"),
            format!(
                "{}: {}\n",
                script.join("out/build_script_build").display(),
                pkg.join("build.rs").display()
            ),
        )
        .unwrap();
        unit(
            &p,
            "sys",
            "00000000000000d2",
            "run-build-script-build-script-build",
            11,
            &[10],
            json!([{"RerunIfChanged": {"output": "x", "paths": ["build.rs"]}}]),
            &[("stdout", b"".as_slice())],
        );
        unit(
            &p,
            "sys",
            "00000000000000d3",
            "lib-sys",
            12,
            &[11],
            json!([{"CheckDepInfo": {"dep_info": "x", "fingerprint": "mtime"}}]),
            &[("libsys-00000000000000d3.rlib", big.as_slice())],
        );
        let none = AtomicBool::new(false);
        let pass = share(&store, &p, &HashSet::new(), &none).unwrap();
        assert_eq!(pass.shared_units, 1, "only the build script's compilation");
        assert!(
            mtime(&p.join("build/sys/00000000000000d3/out/libsys-00000000000000d3.rlib"))
                > BASE_MTIME + 10
        );
        // With the watched file as old as a downloaded crate's, all three share.
        let old = UNIX_EPOCH + Duration::from_secs(1_153_704_088);
        OpenOptions::new()
            .write(true)
            .open(pkg.join("build.rs"))
            .unwrap()
            .set_modified(old)
            .unwrap();
        let pass = share(&store, &p, &HashSet::new(), &none).unwrap();
        assert_eq!(pass.shared_units, 3);
        assert_eq!(
            mtime(&p.join("build/sys/00000000000000d2/run/stdout")),
            BASE_MTIME + 1
        );
        assert_eq!(
            mtime(&p.join("build/sys/00000000000000d3/out/libsys-00000000000000d3.rlib")),
            BASE_MTIME + 2
        );
        fs::remove_dir_all(&t).unwrap();
    }

    #[test]
    fn a_busy_build_directory_is_left_alone() {
        if !lock::AVAILABLE {
            return;
        }
        let t = temp("busy");
        let store = Store::open_at(&t.join("store")).unwrap();
        let (one, _) = two_projects(&t);
        let held = lock::try_exclusive(&one.join(".cargo-build-lock"), false).unwrap();
        assert!(held.is_some());
        let none = AtomicBool::new(false);
        assert_eq!(share(&store, &one, &HashSet::new(), &none), Err(Skip::Busy));
        drop(held);
        assert!(share(&store, &one, &HashSet::new(), &none).is_ok());
        fs::remove_dir_all(&t).unwrap();
    }

    #[test]
    fn unused_objects_go_and_idle_builds_are_trimmed_to_the_budget() {
        if !lock::AVAILABLE {
            return;
        }
        let t = temp("gc");
        let store = Store::open_at(&t.join("store")).unwrap();
        let (one, two) = two_projects(&t);
        let local: HashSet<String> = ["app".to_string()].into();
        let none = AtomicBool::new(false);
        share(&store, &one, &local, &none).unwrap();
        share(&store, &two, &local, &none).unwrap();
        let build = |p: &Path| p.parent().unwrap().to_path_buf();
        note_built(&store, &build(&one), &t.join("one")).unwrap();
        note_built(&store, &build(&two), &t.join("two")).unwrap();
        // Nothing to do under the budget.
        let g = gc(&store, Some(1 << 30), None, false, &none);
        assert_eq!((g.unused_objects, g.trimmed.len()), (0, 0));
        // Over it: only `one` is trimmed (what another compiler built: these
        // fingerprints name a made-up compiler), never `two`, the kept one.
        let g = gc(&store, Some(1), Some(&build(&two)), false, &none);
        assert!(
            g.trimmed
                .iter()
                .all(|(d, _, why)| d.ends_with("one/target") && !why.contains("idle")),
            "{:?}",
            g.trimmed
        );
        assert!(two.join("build/base").is_dir());
        // Make `one` idle: what is left of its build goes too.
        let long_ago = UNIX_EPOCH + Duration::from_secs(now() - 30 * 86_400);
        for u in scan(&one) {
            let ts = u.dir.join("fingerprint/invoked.timestamp");
            OpenOptions::new()
                .write(true)
                .open(&ts)
                .unwrap()
                .set_modified(long_ago)
                .unwrap();
        }
        update_registry(&store, |r| {
            for p in &mut r.projects {
                p.last_built = 0;
            }
        })
        .unwrap();
        gc(&store, Some(1), Some(&build(&two)), false, &none);
        assert!(!one.exists());
        assert!(two.join("build/base").is_dir());
        // The rlibs are still linked by `two`: no object was unused.
        assert_eq!(usage(&store).objects, 2);
        // Without `two`, they are.
        fs::remove_dir_all(build(&two)).unwrap();
        let g = gc(&store, None, None, false, &none);
        assert_eq!(g.unused_objects, 2);
        assert_eq!(g.forgotten.len(), 1);
        assert_eq!(usage(&store).objects, 0);
        fs::remove_dir_all(&t).unwrap();
    }

    /// Whether the store shares a file of this kind. The test's program is
    /// `b`, where cargo's is `build_script_build`.
    fn shared_kind(path: &Path) -> bool {
        let name = path.file_name().unwrap().to_string_lossy().to_lowercase();
        match name.strip_suffix(".exe").unwrap_or(&name) {
            "b" => rustc_writes(Path::new(&name.replacen('b', "build_script_build", 1))),
            _ => rustc_writes(path),
        }
    }

    /// The store is safe only if a build never writes into an output it
    /// shares (a shared file would change under every project). On each
    /// platform: an rlib, an rmeta, a proc-macro and a binary, each a hard
    /// link to a "store" copy, are built again with different code; the
    /// store copies of every kind [`rustc_writes`] shares must not change.
    #[test]
    fn rustc_replaces_its_outputs() {
        let rustc = std::env::var("RUSTC").unwrap_or_else(|_| "rustc".into());
        if std::process::Command::new(&rustc)
            .arg("--version")
            .output()
            .is_err()
        {
            return; // a container without a toolchain
        }
        let t = temp("rustc-replaces");
        let out = t.join("out");
        let keep = t.join("keep");
        fs::create_dir_all(&out).unwrap();
        fs::create_dir_all(&keep).unwrap();
        let build = |n: u32| {
            fs::write(t.join("l.rs"), format!("pub fn f() -> u32 {{ {n} }}")).unwrap();
            fs::write(
                t.join("pm.rs"),
                format!("extern crate proc_macro;\n#[proc_macro]\npub fn m(_: proc_macro::TokenStream) -> proc_macro::TokenStream {{ \"{n}\".parse().unwrap() }}"),
            )
            .unwrap();
            fs::write(
                t.join("b.rs"),
                format!("fn main() {{ println!(\"{n}\"); }}"),
            )
            .unwrap();
            for args in [
                &[
                    "--crate-type",
                    "rlib",
                    "--emit=link,metadata",
                    "--crate-name",
                    "l",
                    "l.rs",
                ][..],
                &["--crate-type", "proc-macro", "--crate-name", "pm", "pm.rs"][..],
                &["--crate-name", "b", "b.rs"][..],
            ] {
                let status = std::process::Command::new(
                    std::env::var("RUSTC").unwrap_or_else(|_| "rustc".into()),
                )
                .args(args)
                .arg("--out-dir")
                .arg(&out)
                .current_dir(&t)
                .status()
                .unwrap();
                assert!(status.success(), "rustc {args:?}");
            }
        };
        build(1);
        let mut kept = Vec::new();
        for e in entries(&out) {
            let p = e.path();
            let name = e.file_name();
            let lower = name.to_string_lossy().to_lowercase();
            if lower.ends_with(".o")
                || lower.ends_with(".pdb")
                || lower.ends_with(".lib")
                || lower.ends_with(".exp")
            {
                continue;
            }
            let k = keep.join(&name);
            fs::rename(&p, &k).unwrap();
            fs::hard_link(&k, &p).unwrap();
            kept.push((k.clone(), fs::read(&k).unwrap()));
        }
        // What the store would share: at least a library and its metadata,
        // and on Linux and macOS the proc-macro library and the program too.
        let shared = kept.iter().filter(|(k, _)| shared_kind(k)).count();
        assert!(shared >= if cfg!(windows) { 2 } else { 4 }, "{kept:?}");
        build(2);
        let written: Vec<_> = kept
            .iter()
            .filter(|(k, before)| shared_kind(k) && fs::read(k).unwrap() != *before)
            .map(|(k, _)| k.display().to_string())
            .collect();
        assert!(written.is_empty(), "written into in place: {written:?}");
        fs::remove_dir_all(&t).unwrap();
    }
}
