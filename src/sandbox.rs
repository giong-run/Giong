//! A sandbox that keeps io_uring: [`Builder::sandbox`](crate::Builder::sandbox).
//!
//! A server that is compromised (a bug in a handler, a parser, a dependency)
//! can do whatever its process can. The usual answer on Linux is to shrink the
//! process after start-up with seccomp, but io_uring's operations (`openat`,
//! `connect`, `unlinkat`, ...) run inside the kernel without passing through
//! seccomp, which is why container runtimes block io_uring outright. giong's
//! sandbox restricts io_uring itself instead, so a service keeps its speed.
//!
//! The sandbox is made of layers, each applied where the kernel has it:
//!
//! - [`Layer::RingRestrictions`] (every kernel giong's io_uring driver runs
//!   on, Linux 6.0 and newer): each worker's ring is created disabled,
//!   limited to the operations giong submits for what the sandbox allows
//!   (`IORING_REGISTER_RESTRICTIONS`), then enabled. The limit cannot be
//!   lifted: an operation outside it fails in the kernel with `EACCES`.
//! - [`Layer::TaskRestrictions`] (Linux 7.0): the same limit on every ring a
//!   covered thread, or any thread it starts later, creates from then on, so
//!   that a compromised process cannot make a ring of its own without it.
//!   giong sets `PR_SET_NO_NEW_PRIVS` on the thread first, as the kernel
//!   requires.
//! - [`Layer::OpenFilters`] (Linux 7.0, only for [`FileAccess::ReadOnly`]):
//!   classic BPF filters on `openat` and `openat2` that refuse an open for
//!   writing, creating or truncating, on the workers' rings and on new rings.
//!
//! Covered threads are the thread that calls [`Runtime::run`] or
//! [`Builder::block_on`], every worker, and every thread they or giong's
//! thread pools start after. The kernel keeps these restrictions for the life
//! of each thread: they outlive the runtime and cannot be lifted. Child
//! processes inherit the per-task restrictions too, except across `execve`
//! on Linux 7.0 and 7.1, which drop them (fixed in 7.2).
//!
//! What the sandbox allows:
//!
//! - every operation giong performs on a descriptor the process already has
//!   (sockets, files, pipes): receiving, sending, reading, writing, accepting,
//!   connecting, closing, `fsync`, `fallocate`, reading an open file's
//!   metadata (`statx`, which the kernel layers therefore allow by path too;
//!   giong refuses a `metadata` by path itself);
//! - operations by path (opening, `metadata`, creating directories, removing,
//!   renaming) only as far as [`FileAccess`] allows, [`FileAccess::None`] by
//!   default. giong refuses the others itself, before the kernel is asked,
//!   with [`Code::SandboxDenied`] and the rule that refused it.
//!
//! The sandbox restricts io_uring only, for now: system calls made directly
//! (by `std::fs`, a C library, or the epoll driver's thread pool) are not
//! limited by it. Seccomp, Landlock and dropping privileges are the next
//! layers.
//!
//! Three modes say what happens when a layer cannot be applied:
//! [`Sandbox::require`] fails start with [`Code::SandboxUnavailable`],
//! [`Sandbox::best_effort`] applies what it can and says on stderr what it
//! could not, and [`Sandbox::audit`] applies nothing and logs what would be
//! refused, to write the rules of a service without breaking it.
//! [`report`] says what a worker got.
//!
//! ```no_run
//! use giong::sandbox::{FileAccess, Sandbox};
//!
//! giong::Builder::new()
//!     .sandbox(Sandbox::best_effort().files(FileAccess::ReadOnly))
//!     .build()?
//!     .run(|_| async {
//!         // Serving static files works; writing one is refused.
//!         let _ = giong::fs::read("static/index.html").await;
//!         assert!(giong::fs::write("static/x", &b"no"[..]).await.is_err());
//!     })?;
//! # Ok::<(), std::io::Error>(())
//! ```
//!
//! Without [`Builder::sandbox`] nothing of this runs: the builder stores
//! `None`, and no operation pays for it.
//!
//! [`Code::SandboxDenied`]: crate::error::Code::SandboxDenied
//! [`Code::SandboxUnavailable`]: crate::error::Code::SandboxUnavailable
//! [`Runtime::run`]: crate::Runtime::run
//! [`Builder::block_on`]: crate::Builder::block_on
//! [`Builder::sandbox`]: crate::Builder::sandbox

use std::collections::HashSet;
use std::ffi::CStr;
use std::io;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::Mutex;

use crate::error::{Code, Error};

#[cfg(target_os = "linux")]
pub(crate) mod linux;

/// What a runtime's sandbox allows, and what happens when the kernel lacks
/// a layer. Given to [`Builder::sandbox`](crate::Builder::sandbox).
///
/// Built with [`require`](Self::require), [`best_effort`](Self::best_effort)
/// or [`audit`](Self::audit), then narrowed or widened with its methods.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[must_use = "a sandbox does nothing until it is given to Builder::sandbox"]
pub struct Sandbox {
    mode: Mode,
    files: FileAccess,
}

/// What a [`Sandbox`] does when the kernel lacks a layer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Mode {
    /// Every layer the sandbox needs must be applied, or the runtime does not
    /// start ([`Sandbox::require`]).
    Require,
    /// The layers the kernel has are applied; the others are reported
    /// ([`Sandbox::best_effort`]).
    BestEffort,
    /// Nothing is applied; what would be refused is logged
    /// ([`Sandbox::audit`]).
    Audit,
}

/// Which operations by path a [`Sandbox`] allows through giong.
///
/// Operations on descriptors the process already has (reading and writing an
/// open file) are not limited by it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Default)]
#[non_exhaustive]
pub enum FileAccess {
    /// No operation by path: no open, no metadata, no change. The default.
    #[default]
    None,
    /// Opening files for reading and reading their metadata. Opening for
    /// writing, creating, truncating, removing, renaming and creating
    /// directories are refused. Enforced by the kernel through
    /// [`Layer::OpenFilters`] (Linux 7.0).
    ReadOnly,
    /// Every operation by path giong has.
    ReadWrite,
}

impl Sandbox {
    /// A sandbox that must be applied whole: the runtime fails to start, with
    /// [`Code::SandboxUnavailable`],
    /// on a kernel or platform that lacks a layer it needs. Allows
    /// [`FileAccess::None`] until [`files`](Self::files) says otherwise.
    ///
    /// Needs Linux 7.0 for now: without [`Layer::TaskRestrictions`] a
    /// compromised process could create a ring that is not restricted.
    pub fn require() -> Self {
        Self {
            mode: Mode::Require,
            files: FileAccess::None,
        }
    }

    /// A sandbox applied as far as the kernel allows: each layer the kernel
    /// lacks is said once on stderr and shows as
    /// [`LayerState::Unavailable`] in [`report`]. Allows [`FileAccess::None`]
    /// until [`files`](Self::files) says otherwise.
    pub fn best_effort() -> Self {
        Self {
            mode: Mode::BestEffort,
            files: FileAccess::None,
        }
    }

    /// A sandbox that refuses nothing and restricts nothing: each operation it
    /// would refuse is logged once on stderr as one JSON line
    /// (`{"giong":"sandbox-audit",...}`), with the rule and the path. Use it to
    /// find the [`FileAccess`] a service needs, then switch to
    /// [`require`](Self::require) or [`best_effort`](Self::best_effort).
    pub fn audit() -> Self {
        Self {
            mode: Mode::Audit,
            files: FileAccess::None,
        }
    }

    /// Sets which operations by path are allowed (see [`FileAccess`]).
    pub fn files(mut self, access: FileAccess) -> Self {
        self.files = access;
        self
    }

    /// The mode this sandbox was built with.
    pub fn mode(&self) -> Mode {
        self.mode
    }

    /// The operations by path it allows.
    pub fn file_access(&self) -> FileAccess {
        self.files
    }
}

/// A layer of the sandbox (see the [module documentation](self)).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Layer {
    /// The worker's own ring is limited to the operations giong submits
    /// (`IORING_REGISTER_RESTRICTIONS`).
    RingRestrictions,
    /// Every ring the thread creates from now on is limited the same way
    /// (Linux 7.0's per-task io_uring restrictions).
    TaskRestrictions,
    /// Opens for writing are refused by classic BPF filters on `openat` and
    /// `openat2` (Linux 7.0's `IORING_REGISTER_BPF_FILTER`).
    OpenFilters,
}

/// How a [`Layer`] went.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum LayerState {
    /// The kernel enforces it.
    Applied,
    /// Not needed here: a worker on the epoll driver has no ring to restrict,
    /// and only [`FileAccess::ReadOnly`] needs [`Layer::OpenFilters`].
    NotNeeded,
    /// The kernel or the platform does not have it. The detail says why.
    Unavailable,
    /// Not applied, because the sandbox is in audit mode.
    Audit,
}

/// One line of a [`Report`].
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct LayerReport {
    /// The layer.
    pub layer: Layer,
    /// How it went.
    pub state: LayerState,
    /// What was done, or why it could not be, in a sentence.
    pub detail: String,
}

/// What the sandbox of the current worker's runtime got: see [`report`].
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct Report {
    /// The sandbox's mode.
    pub mode: Mode,
    /// The operations by path it allows.
    pub files: FileAccess,
    /// Every layer, applied or not, in the order of [`Layer`].
    pub layers: Vec<LayerReport>,
}

impl Report {
    /// The line for `layer`.
    pub fn layer(&self, layer: Layer) -> Option<&LayerReport> {
        self.layers.iter().find(|l| l.layer == layer)
    }

    /// `true` when every layer is applied or not needed.
    pub fn is_complete(&self) -> bool {
        self.layers
            .iter()
            .all(|l| matches!(l.state, LayerState::Applied | LayerState::NotNeeded))
    }
}

/// What the sandbox of the current worker's runtime got, or `None` when the
/// runtime has no sandbox.
///
/// # Panics
///
/// Outside a giong runtime, with
/// [`Code::NotInRuntime`].
pub fn report() -> Option<Report> {
    crate::runtime::with_worker(|w| w.sandbox().map(|s| s.report.clone()))
}

/// The sandbox `GIONG_SANDBOX` asks for, for a runtime that has none of its
/// own: `none`, `read-only` or `read-write` give a [`Sandbox::best_effort`]
/// with that [`FileAccess`]; unset, empty or `off`, none.
pub(crate) fn from_env() -> io::Result<Option<Sandbox>> {
    let Ok(v) = std::env::var("GIONG_SANDBOX") else {
        return Ok(None);
    };
    let files = match v.trim() {
        "" | "off" => return Ok(None),
        "none" => FileAccess::None,
        "read-only" => FileAccess::ReadOnly,
        "read-write" => FileAccess::ReadWrite,
        other => {
            return Err(crate::error::raise(
                Code::InvalidConfig,
                format!("GIONG_SANDBOX is {other:?}, which is not a sandbox setting"),
                "set GIONG_SANDBOX to none, read-only or read-write (a best-effort sandbox \
                 allowing that file access), or unset it",
            ))
        }
    };
    Ok(Some(Sandbox::best_effort().files(files)))
}

/// The sandbox of one worker: what it allows and what it got.
#[derive(Debug)]
pub(crate) struct WorkerSandbox {
    pub(crate) sandbox: Sandbox,
    pub(crate) report: Report,
}

/// What an operation by path needs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Need {
    /// Reading: an open for reading, metadata.
    Read,
    /// Changing: an open for writing, creating or truncating; removing,
    /// renaming, creating a directory.
    Write,
}

impl Need {
    /// What an open with `flags` needs.
    pub(crate) fn of_open(flags: i32) -> Need {
        if flags & write_open_flags() != 0 {
            Need::Write
        } else {
            Need::Read
        }
    }

    fn allowed_by(self, files: FileAccess) -> bool {
        match self {
            Need::Read => files >= FileAccess::ReadOnly,
            Need::Write => files >= FileAccess::ReadWrite,
        }
    }
}

/// The open flags that make an open one for writing: the access mode bits
/// (`O_WRONLY`, `O_RDWR`), `O_CREAT`, `O_TRUNC` and `O_TMPFILE` without the
/// `O_DIRECTORY` bit it shares (opening a directory is reading). The same
/// mask the kernel's [`Layer::OpenFilters`] test.
pub(crate) const fn write_open_flags() -> i32 {
    #[cfg(target_os = "linux")]
    {
        libc::O_ACCMODE | libc::O_CREAT | libc::O_TRUNC | (libc::O_TMPFILE & !libc::O_DIRECTORY)
    }
    #[cfg(all(unix, not(target_os = "linux")))]
    {
        libc::O_ACCMODE | libc::O_CREAT | libc::O_TRUNC
    }
    #[cfg(windows)]
    {
        // Windows' opens do not go through these flags (see fs/windows.rs).
        0
    }
}

/// Checks an operation by path against the sandbox of the current worker's
/// runtime: `what` names it (`"open for writing"`), `path` is its path.
/// Without a sandbox this is one look at the worker.
pub(crate) fn check(need: Need, what: &'static str, path: &CStr) -> io::Result<()> {
    let Some(sandbox) =
        crate::runtime::try_with_worker(|w| w.sandbox().map(|s| s.sandbox)).flatten()
    else {
        return Ok(());
    };
    if need.allowed_by(sandbox.files) {
        return Ok(());
    }
    if sandbox.mode == Mode::Audit {
        audit_log(sandbox.files, need, what, path);
        return Ok(());
    }
    Err(Error::new(
        Code::SandboxDenied,
        format!(
            "the sandbox refused to {what} {}: it allows {} (FileAccess::{:?})",
            path.to_string_lossy(),
            describe(sandbox.files),
            sandbox.files,
        ),
        format!(
            "if the service needs this, allow it: Sandbox::files(FileAccess::{:?}) on the \
             Builder::sandbox; if it should not happen, find what asked for {}",
            needed_access(need),
            path.to_string_lossy(),
        ),
    )
    .into())
}

fn needed_access(need: Need) -> FileAccess {
    match need {
        Need::Read => FileAccess::ReadOnly,
        Need::Write => FileAccess::ReadWrite,
    }
}

fn describe(files: FileAccess) -> &'static str {
    match files {
        FileAccess::None => "no operation by path",
        FileAccess::ReadOnly => "reading files only",
        FileAccess::ReadWrite => "every operation by path",
    }
}

/// How many distinct refusals audit mode logs before it stops (with one
/// last line saying so), so that a service that opens a new path per
/// request cannot fill its log.
const AUDIT_LIMIT: usize = 256;

/// The cases audit mode logged: the operation and the path.
type Seen = HashSet<(&'static str, Vec<u8>)>;

fn audit_log(files: FileAccess, need: Need, what: &'static str, path: &CStr) {
    static SEEN: Mutex<Option<Seen>> = Mutex::new(None);
    let mut seen = SEEN.lock().unwrap_or_else(|e| e.into_inner());
    let seen = seen.get_or_insert_with(HashSet::new);
    if seen.len() > AUDIT_LIMIT {
        return;
    }
    if seen.len() == AUDIT_LIMIT {
        seen.insert(("", Vec::new()));
        eprintln!(
            "{{\"giong\":\"sandbox-audit\",\"rule\":\"files\",\"note\":\"{AUDIT_LIMIT} distinct \
             cases logged; further ones are not\"}}"
        );
        return;
    }
    if !seen.insert((what, path.to_bytes().to_vec())) {
        return;
    }
    eprintln!(
        "{{\"giong\":\"sandbox-audit\",\"rule\":\"files\",\"allowed\":\"{:?}\",\"needs\":\"{:?}\",\
         \"op\":\"{what}\",\"path\":{}}}",
        files,
        needed_access(need),
        json_string(&path.to_string_lossy()),
    );
}

/// `s` as a JSON string, quotes included.
fn json_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// The [`FileAccess`] of the thread layer already applied in this process,
/// plus one (0: none yet), for [`cover_thread`] and for runtimes that start
/// later.
static THREAD_POLICY: AtomicU8 = AtomicU8::new(0);

fn encode(files: FileAccess) -> u8 {
    match files {
        FileAccess::None => 1,
        FileAccess::ReadOnly => 2,
        FileAccess::ReadWrite => 3,
    }
}

fn decode(v: u8) -> Option<FileAccess> {
    match v {
        1 => Some(FileAccess::None),
        2 => Some(FileAccess::ReadOnly),
        3 => Some(FileAccess::ReadWrite),
        _ => None,
    }
}

/// Applies the thread layers (per-task restrictions and their open
/// filters) to the calling thread. `ring` is what the worker's ring got,
/// if the calling thread is a worker on io_uring.
///
/// Returns the report lines for [`Layer::TaskRestrictions`] and
/// [`Layer::OpenFilters`] (merged with the ring's), or an error when another
/// runtime of this process already applied a different sandbox and this one
/// requires its own.
pub(crate) fn apply_thread(sandbox: &Sandbox) -> io::Result<Vec<LayerReport>> {
    if sandbox.mode == Mode::Audit {
        return Ok(vec![
            audit_line(Layer::TaskRestrictions),
            audit_line(Layer::OpenFilters),
        ]);
    }
    let files = sandbox.files;
    match THREAD_POLICY.compare_exchange(0, encode(files), Ordering::AcqRel, Ordering::Acquire) {
        Ok(_) => {}
        Err(v) if decode(v) == Some(files) => {}
        Err(v) => {
            let other = decode(v).unwrap_or_default();
            if sandbox.mode == Mode::Require {
                return Err(conflict(files, other));
            }
            return Ok(vec![LayerReport {
                layer: Layer::TaskRestrictions,
                state: LayerState::Unavailable,
                detail: format!(
                    "another runtime of this process already restricted its threads with \
                     FileAccess::{other:?}, and a thread keeps the first restriction it gets"
                ),
            }]);
        }
    }
    Ok(apply_thread_here(files))
}

#[cfg(target_os = "linux")]
fn apply_thread_here(files: FileAccess) -> Vec<LayerReport> {
    linux::apply_thread(files)
}

#[cfg(not(target_os = "linux"))]
fn apply_thread_here(_files: FileAccess) -> Vec<LayerReport> {
    vec![
        LayerReport {
            layer: Layer::TaskRestrictions,
            state: LayerState::Unavailable,
            detail: "io_uring and its restrictions are Linux only".into(),
        },
        LayerReport {
            layer: Layer::OpenFilters,
            state: LayerState::Unavailable,
            detail: "io_uring and its filters are Linux only".into(),
        },
    ]
}

/// Gives a thread that giong starts (a blocking or compute thread) the
/// thread layer this process applied, if any. A thread started by a covered
/// thread already has it from its parent; this covers the others. Without a
/// sandbox it is one atomic load.
pub(crate) fn cover_thread() {
    let Some(files) = decode(THREAD_POLICY.load(Ordering::Acquire)) else {
        return;
    };
    #[cfg(target_os = "linux")]
    {
        let _ = linux::apply_thread(files);
    }
    #[cfg(not(target_os = "linux"))]
    let _ = files;
}

fn audit_line(layer: Layer) -> LayerReport {
    LayerReport {
        layer,
        state: LayerState::Audit,
        detail: "audit mode: not applied; refusals are logged instead".into(),
    }
}

/// The report line for a worker without a ring (epoll, kqueue, IOCP).
pub(crate) fn no_ring(sandbox: &Sandbox) -> LayerReport {
    if sandbox.mode == Mode::Audit {
        return audit_line(Layer::RingRestrictions);
    }
    #[cfg(target_os = "linux")]
    let (state, detail) = (
        LayerState::NotNeeded,
        "this worker runs on the epoll driver: it has no ring to restrict",
    );
    #[cfg(not(target_os = "linux"))]
    let (state, detail) = (
        LayerState::Unavailable,
        "io_uring and its restrictions are Linux only",
    );
    LayerReport {
        layer: Layer::RingRestrictions,
        state,
        detail: detail.into(),
    }
}

/// Puts the report of a worker together from what its ring got (`ring`:
/// the [`Layer::RingRestrictions`] line and, when it had to filter opens,
/// its [`Layer::OpenFilters`] line) and what its thread got, and decides:
/// a [`Mode::Require`] sandbox missing a layer it needs is an error;
/// a [`Mode::BestEffort`] one says on stderr, once per process, what it
/// could not apply.
pub(crate) fn finish(
    sandbox: &Sandbox,
    ring: Vec<LayerReport>,
    thread: Vec<LayerReport>,
) -> io::Result<WorkerSandbox> {
    let mut layers: Vec<LayerReport> = Vec::with_capacity(3);
    for layer in [
        Layer::RingRestrictions,
        Layer::TaskRestrictions,
        Layer::OpenFilters,
    ] {
        let mut lines = ring
            .iter()
            .chain(thread.iter())
            .filter(|l| l.layer == layer);
        let line = match layer {
            // The filters must be wherever opens can be submitted: on the
            // worker's ring and on new rings. The worst of the two counts.
            Layer::OpenFilters => lines.max_by_key(|l| rank(l.state)).cloned(),
            _ => lines.next().cloned(),
        };
        layers.push(line.unwrap_or_else(|| LayerReport {
            layer,
            state: LayerState::NotNeeded,
            detail: "not needed for this sandbox".into(),
        }));
    }
    if sandbox.files != FileAccess::ReadOnly {
        let filters = &mut layers[2];
        if filters.state != LayerState::Audit {
            filters.state = LayerState::NotNeeded;
            filters.detail = "only FileAccess::ReadOnly needs open filters: the other \
                              settings allow or refuse opens whole"
                .into();
        }
    }
    let report = Report {
        mode: sandbox.mode,
        files: sandbox.files,
        layers,
    };
    let missing: Vec<&LayerReport> = report
        .layers
        .iter()
        .filter(|l| l.state == LayerState::Unavailable)
        .collect();
    if !missing.is_empty() {
        match sandbox.mode {
            Mode::Require => return Err(unavailable(&missing)),
            Mode::BestEffort => say_missing_once(&missing),
            Mode::Audit => {}
        }
    }
    Ok(WorkerSandbox {
        sandbox: *sandbox,
        report,
    })
}

/// Orders states from best to worst, for [`finish`].
fn rank(state: LayerState) -> u8 {
    match state {
        LayerState::NotNeeded => 0,
        LayerState::Applied => 1,
        LayerState::Audit => 2,
        LayerState::Unavailable => 3,
    }
}

fn say_missing_once(missing: &[&LayerReport]) {
    static SAID: AtomicU8 = AtomicU8::new(0);
    if SAID.swap(1, Ordering::Relaxed) != 0 {
        return;
    }
    let list: Vec<String> = missing
        .iter()
        .map(|l| format!("{:?} ({})", l.layer, l.detail))
        .collect();
    eprintln!(
        "giong: the sandbox runs without {} (Sandbox::best_effort; giong::sandbox::report() \
         tells what each worker got)",
        list.join("; ")
    );
}

/// The error of a [`Mode::Require`] sandbox missing `missing`.
pub(crate) fn unavailable(missing: &[&LayerReport]) -> io::Error {
    let list: Vec<String> = missing
        .iter()
        .map(|l| format!("{:?}: {}", l.layer, l.detail))
        .collect();
    Error::new(
        Code::SandboxUnavailable,
        format!(
            "the sandbox cannot be applied whole here, and Sandbox::require asks for it: {}",
            list.join("; ")
        ),
        "run on a kernel that has the missing layers (Linux 7.0 for per-task io_uring \
         restrictions and open filters), or use Sandbox::best_effort to run with the layers \
         this kernel has; giong::sandbox::report() then says which",
    )
    .into()
}

fn conflict(wanted: FileAccess, applied: FileAccess) -> io::Error {
    Error::new(
        Code::SandboxConflict,
        format!(
            "this runtime's sandbox allows FileAccess::{wanted:?}, but another runtime of this \
             process already restricted the threads with FileAccess::{applied:?}, and a thread \
             keeps the first restriction it gets"
        ),
        "give every runtime of the process the same Sandbox, or start the sandboxed runtime in \
         its own process",
    )
    .into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn needs_follow_the_open_flags() {
        assert_eq!(Need::of_open(libc::O_RDONLY), Need::Read);
        assert_eq!(Need::of_open(libc::O_RDONLY | libc::O_CLOEXEC), Need::Read);
        #[cfg(target_os = "linux")]
        assert_eq!(
            Need::of_open(libc::O_RDONLY | libc::O_DIRECTORY),
            Need::Read
        );
        assert_eq!(Need::of_open(libc::O_WRONLY), Need::Write);
        assert_eq!(Need::of_open(libc::O_RDWR), Need::Write);
        assert_eq!(Need::of_open(libc::O_RDONLY | libc::O_CREAT), Need::Write);
        assert_eq!(Need::of_open(libc::O_RDONLY | libc::O_TRUNC), Need::Write);
        #[cfg(target_os = "linux")]
        assert_eq!(Need::of_open(libc::O_RDWR | libc::O_TMPFILE), Need::Write);
    }

    #[test]
    fn access_levels_allow_what_they_say() {
        assert!(!Need::Read.allowed_by(FileAccess::None));
        assert!(Need::Read.allowed_by(FileAccess::ReadOnly));
        assert!(!Need::Write.allowed_by(FileAccess::ReadOnly));
        assert!(Need::Write.allowed_by(FileAccess::ReadWrite));
    }

    #[test]
    fn json_strings_are_escaped() {
        assert_eq!(json_string("a\"b\\c\nd"), "\"a\\\"b\\\\c\\u000ad\"");
    }

    #[test]
    fn require_fails_on_a_missing_layer_and_best_effort_does_not() {
        let missing = |sandbox: Sandbox| {
            finish(
                &sandbox,
                vec![LayerReport {
                    layer: Layer::RingRestrictions,
                    state: LayerState::Applied,
                    detail: String::new(),
                }],
                vec![LayerReport {
                    layer: Layer::TaskRestrictions,
                    state: LayerState::Unavailable,
                    detail: "test".into(),
                }],
            )
        };
        let e = missing(Sandbox::require()).unwrap_err();
        assert_eq!(crate::error::code(&e), Some(Code::SandboxUnavailable));
        let ok = missing(Sandbox::best_effort()).unwrap();
        assert!(!ok.report.is_complete());
        assert_eq!(
            ok.report.layer(Layer::OpenFilters).unwrap().state,
            LayerState::NotNeeded
        );
    }

    #[test]
    fn read_only_takes_the_worst_of_the_ring_and_thread_filters() {
        let line = |layer, state| LayerReport {
            layer,
            state,
            detail: String::new(),
        };
        let w = finish(
            &Sandbox::best_effort().files(FileAccess::ReadOnly),
            vec![
                line(Layer::RingRestrictions, LayerState::Applied),
                line(Layer::OpenFilters, LayerState::Applied),
            ],
            vec![
                line(Layer::TaskRestrictions, LayerState::Applied),
                line(Layer::OpenFilters, LayerState::Unavailable),
            ],
        )
        .unwrap();
        assert_eq!(
            w.report.layer(Layer::OpenFilters).unwrap().state,
            LayerState::Unavailable
        );
    }
}
