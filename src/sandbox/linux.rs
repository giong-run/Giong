//! The Linux layers of the sandbox: io_uring restrictions on a ring
//! (`IORING_REGISTER_RESTRICTIONS`, Linux 5.10), per-task restrictions and
//! classic BPF filters on opens (Linux 7.0's "blind" `io_uring_register`
//! with no ring, and `IORING_REGISTER_BPF_FILTER`).
//!
//! The kernel's interfaces, from `include/uapi/linux/io_uring.h` and
//! `include/uapi/linux/io_uring/bpf_filter.h`:
//!
//! - restrictions are an array of `struct io_uring_restriction`, applied once
//!   to a ring created with `IORING_SETUP_R_DISABLED`, which is then enabled
//!   with `IORING_REGISTER_ENABLE_RINGS`; or, with the ring descriptor -1, a
//!   `struct io_uring_task_restriction` (a header and the same array) on the
//!   calling task, copied into every ring it creates later and into the
//!   tasks it starts. The task form needs `no_new_privs` or `CAP_SYS_ADMIN`
//!   and may be given once per task (`EPERM` after);
//! - a filter is a classic BPF program run on a `struct io_uring_bpf_ctx`
//!   for one opcode; it returns 0 to refuse the operation (`EACCES`) and 1
//!   to allow it. For `openat` and `openat2` the context holds the open's
//!   flags, mode and resolve flags. Filters only ever add up.

use std::io;
use std::os::fd::RawFd;

use io_uring::{opcode, squeue};

use super::{FileAccess, Layer, LayerReport, LayerState};

const REGISTER_PROBE: u8 = 8;
const REGISTER_RESTRICTIONS: u8 = 11;
const REGISTER_ENABLE_RINGS: u8 = 12;
const REGISTER_RING_FDS: u8 = 20;
const UNREGISTER_RING_FDS: u8 = 21;
const REGISTER_PBUF_RING: u8 = 22;
const UNREGISTER_PBUF_RING: u8 = 23;
const REGISTER_SYNC_CANCEL: u8 = 24;
const REGISTER_BPF_FILTER: u8 = 37;

/// What giong registers on its rings after they are enabled: a probe for
/// zero-copy sends, the registered ring descriptor, the buffer rings, and
/// the cancellation at shutdown.
const RING_REGISTER_OPS: &[u8] = &[
    REGISTER_PROBE,
    REGISTER_RING_FDS,
    UNREGISTER_RING_FDS,
    REGISTER_PBUF_RING,
    UNREGISTER_PBUF_RING,
    REGISTER_SYNC_CANCEL,
];

/// What a thread with per-task restrictions may register. These apply to
/// every ring it creates later even while the ring is disabled, so they
/// hold what setting up a ring takes ([`RING_REGISTER_OPS`], enabling it)
/// and what only narrows (restrictions and filters, so that a later
/// runtime on the thread can probe and add its own).
const TASK_REGISTER_OPS: &[u8] = &[
    REGISTER_PROBE,
    REGISTER_RESTRICTIONS,
    REGISTER_ENABLE_RINGS,
    REGISTER_RING_FDS,
    UNREGISTER_RING_FDS,
    REGISTER_PBUF_RING,
    UNREGISTER_PBUF_RING,
    REGISTER_SYNC_CANCEL,
    REGISTER_BPF_FILTER,
];

/// `IORING_RESTRICTION_*`.
const RESTRICTION_REGISTER_OP: u16 = 0;
const RESTRICTION_SQE_OP: u16 = 1;
const RESTRICTION_SQE_FLAGS_ALLOWED: u16 = 2;

/// `struct io_uring_restriction`.
#[repr(C)]
#[derive(Clone, Copy, Default, Debug)]
struct Restriction {
    opcode: u16,
    /// `register_op`, `sqe_op` or `sqe_flags`, by `opcode`.
    arg: u8,
    resv: u8,
    resv2: [u32; 3],
}

const _: () = assert!(std::mem::size_of::<Restriction>() == 16);

/// The most restrictions giong registers at once.
const MAX_RESTRICTIONS: usize = 40;

/// `struct io_uring_task_restriction` with room for [`MAX_RESTRICTIONS`].
#[repr(C)]
struct TaskRestriction {
    flags: u16,
    nr_res: u16,
    resv: [u32; 3],
    restrictions: [Restriction; MAX_RESTRICTIONS],
}

const _: () = assert!(std::mem::size_of::<TaskRestriction>() == 16 + 16 * MAX_RESTRICTIONS);

/// The operations giong submits whatever the sandbox allows: everything on
/// a descriptor the process already has. `statx` is one of them because
/// giong reads an open file's metadata with it (an empty path and
/// `AT_EMPTY_PATH`); the kernel cannot tell that from a `statx` by path,
/// which giong refuses itself under [`FileAccess::None`].
fn descriptor_ops() -> [u8; 16] {
    [
        opcode::Accept::CODE,
        opcode::AsyncCancel::CODE,
        opcode::Close::CODE,
        opcode::Connect::CODE,
        opcode::Fallocate::CODE,
        opcode::Fsync::CODE,
        opcode::PollAdd::CODE,
        opcode::Read::CODE,
        opcode::Recv::CODE,
        opcode::RecvMsg::CODE,
        opcode::Send::CODE,
        opcode::SendMsg::CODE,
        opcode::SendMsgZc::CODE,
        opcode::SendZc::CODE,
        opcode::Statx::CODE,
        opcode::Write::CODE,
    ]
}

/// The operations by path that reading needs.
fn read_path_ops() -> [u8; 2] {
    [opcode::OpenAt::CODE, opcode::OpenAt2::CODE]
}

/// The operations by path that only changing needs.
fn write_path_ops() -> [u8; 3] {
    [
        opcode::MkDirAt::CODE,
        opcode::RenameAt::CODE,
        opcode::UnlinkAt::CODE,
    ]
}

/// The SQE opcodes a ring may run under `files`.
pub(crate) fn allowed_ops(files: FileAccess) -> Vec<u8> {
    let mut ops = descriptor_ops().to_vec();
    if files >= FileAccess::ReadOnly {
        ops.extend(read_path_ops());
    }
    if files >= FileAccess::ReadWrite {
        ops.extend(write_path_ops());
    }
    ops
}

/// The SQE flags giong sets: `BUFFER_SELECT` on receives from the buffer
/// rings, `ASYNC` on a close a test hands to a kernel worker.
pub(crate) fn allowed_sqe_flags() -> u8 {
    (squeue::Flags::BUFFER_SELECT | squeue::Flags::ASYNC).bits()
}

/// The opcodes a kernel has, from `IORING_REGISTER_PROBE` on a ring: the
/// kernel refuses restrictions that name an opcode it does not know (Linux
/// 6.0 has no `IORING_OP_SENDMSG_ZC`), and giong never submits one it does
/// not have.
pub(crate) type Supported = Box<[bool; 256]>;

/// Probes the ring `fd` for the opcodes its kernel has; `None` if the probe
/// fails (then every opcode is named).
fn probe_ops(fd: RawFd) -> Option<Supported> {
    // `struct io_uring_probe`: a 16-byte header, then 256 entries of 8
    // bytes (`op`, `resv`, `flags`, `resv2`).
    let mut buf = vec![0u64; 2 + 256];
    register(fd, REGISTER_PROBE, buf.as_mut_ptr().cast(), 256).ok()?;
    let bytes: Vec<u8> = buf.iter().flat_map(|w| w.to_ne_bytes()).collect();
    let ops_len = bytes[1] as usize;
    let mut supported = Box::new([false; 256]);
    for i in 0..ops_len.min(256) {
        let entry = &bytes[16 + 8 * i..16 + 8 * i + 8];
        // `IO_URING_OP_SUPPORTED`.
        let flags = u16::from_ne_bytes([entry[2], entry[3]]);
        supported[entry[0] as usize] = flags & 1 != 0;
    }
    Some(supported)
}

fn restrictions(
    files: FileAccess,
    register_ops: &[u8],
    supported: Option<&Supported>,
) -> Vec<Restriction> {
    let entry = |opcode, arg| Restriction {
        opcode,
        arg,
        ..Restriction::default()
    };
    let mut res: Vec<Restriction> = allowed_ops(files)
        .into_iter()
        .filter(|&op| match supported {
            Some(s) => s[op as usize],
            None => true,
        })
        .map(|op| entry(RESTRICTION_SQE_OP, op))
        .collect();
    res.push(entry(RESTRICTION_SQE_FLAGS_ALLOWED, allowed_sqe_flags()));
    res.extend(
        register_ops
            .iter()
            .map(|&op| entry(RESTRICTION_REGISTER_OP, op)),
    );
    debug_assert!(res.len() <= MAX_RESTRICTIONS);
    res
}

/// How many SQE opcodes `res` allows.
fn sqe_ops(res: &[Restriction]) -> usize {
    res.iter()
        .filter(|r| r.opcode == RESTRICTION_SQE_OP)
        .count()
}

/// `io_uring_register(2)`: the result, or the error number.
fn register(fd: RawFd, op: u8, arg: *const libc::c_void, nr: u32) -> Result<i32, i32> {
    // SAFETY: every caller passes an `arg` that points at what `op` reads
    // (`nr` items of it), alive for the call, or null where the kernel
    // reads nothing or where the call is a probe that must fail with
    // `EFAULT`; the kernel writes back only into `io_uring_bpf`, which the
    // callers own mutably.
    let ret = unsafe { libc::syscall(libc::SYS_io_uring_register, fd, op as u32, arg, nr) };
    if ret < 0 {
        Err(io::Error::last_os_error().raw_os_error().unwrap_or(0))
    } else {
        Ok(ret as i32)
    }
}

fn errno_name(e: i32) -> String {
    let name = match e {
        libc::EINVAL => "EINVAL",
        libc::EBADF => "EBADF",
        libc::EPERM => "EPERM",
        libc::EACCES => "EACCES",
        libc::EBUSY => "EBUSY",
        libc::EFAULT => "EFAULT",
        libc::EMSGSIZE => "EMSGSIZE",
        libc::ENOSYS => "ENOSYS",
        libc::ENOMEM => "ENOMEM",
        _ => return format!("error {e}"),
    };
    name.to_string()
}

fn kernel_release() -> String {
    // SAFETY: `uname` fills the struct, all-zero being a valid start.
    let mut u: libc::utsname = unsafe { std::mem::zeroed() };
    // SAFETY: `u` is a valid, writable `utsname`.
    if unsafe { libc::uname(&mut u) } != 0 {
        return "unknown".into();
    }
    // SAFETY: the kernel NUL-terminates `release`.
    unsafe { std::ffi::CStr::from_ptr(u.release.as_ptr()) }
        .to_string_lossy()
        .into_owned()
}

fn line(layer: Layer, state: LayerState, detail: impl Into<String>) -> LayerReport {
    LayerReport {
        layer,
        state,
        detail: detail.into(),
    }
}

/// Restricts the ring `fd`, created with `IORING_SETUP_R_DISABLED`, to what
/// `files` allows, adds the open filters [`FileAccess::ReadOnly`] needs, and
/// enables it. Returns the report lines for [`Layer::RingRestrictions`] and,
/// for read-only access, [`Layer::OpenFilters`]. A restriction the kernel
/// refuses is reported, not an error: the ring is enabled without it. Fails
/// only when the ring cannot be enabled.
pub(crate) fn restrict_ring(fd: RawFd, files: FileAccess) -> io::Result<Vec<LayerReport>> {
    let supported = probe_ops(fd);
    let res = restrictions(files, RING_REGISTER_OPS, supported.as_ref());
    let mut lines = vec![match register(
        fd,
        REGISTER_RESTRICTIONS,
        res.as_ptr().cast(),
        res.len() as u32,
    ) {
        Ok(_) => line(
            Layer::RingRestrictions,
            LayerState::Applied,
            format!(
                "the worker's ring runs only the {} operations giong submits for \
                 FileAccess::{files:?}",
                sqe_ops(&res)
            ),
        ),
        // A thread with per-task restrictions creates rings that already
        // have them.
        Err(libc::EBUSY) => line(
            Layer::RingRestrictions,
            LayerState::Applied,
            "the worker's ring was created restricted by its thread's per-task restrictions",
        ),
        Err(e) => line(
            Layer::RingRestrictions,
            LayerState::Unavailable,
            format!(
                "Linux {} refused IORING_REGISTER_RESTRICTIONS ({})",
                kernel_release(),
                errno_name(e)
            ),
        ),
    }];
    if files == FileAccess::ReadOnly {
        lines.push(open_filters(fd, "the worker's ring"));
    }
    register(fd, REGISTER_ENABLE_RINGS, std::ptr::null(), 0)
        .map_err(io::Error::from_raw_os_error)?;
    Ok(lines)
}

/// How a probe for per-task restrictions went.
enum TaskSupport {
    /// The kernel has them, and these opcodes (if the probe worked).
    Yes(Option<Supported>),
    /// The thread has them already, from the thread that started it.
    AlreadyRestricted,
    /// No ring can be created here at all: a seccomp profile (Docker's
    /// default) or `kernel.io_uring_disabled` refuses `io_uring_setup`.
    Blocked,
    /// The kernel does not have them: its error.
    No(i32),
}

/// Asks the kernel whether it has per-task restrictions, without applying
/// any: a null argument makes a kernel that has them fail with `EFAULT`
/// (or `EACCES` without `no_new_privs`) where one that does not fails with
/// `EINVAL` (blind registration but no restrictions) or `EBADF` (no blind
/// registration, before 6.13).
fn task_support() -> TaskSupport {
    let supported = match scratch_ring_ops() {
        Err(()) => return TaskSupport::Blocked,
        Ok(supported) => supported,
    };
    match register(-1, REGISTER_RESTRICTIONS, std::ptr::null(), 1) {
        Err(libc::EFAULT | libc::EACCES) => TaskSupport::Yes(supported),
        Err(libc::EPERM) => TaskSupport::AlreadyRestricted,
        Err(e) => TaskSupport::No(e),
        // A kernel that accepted a null array: nothing to go on.
        Ok(_) => TaskSupport::No(0),
    }
}

/// Creates a ring of one entry and probes the opcodes the kernel has, then
/// closes it. `Err` when `io_uring_setup` is refused to this thread
/// (`EPERM`, `EACCES` or `ENOSYS`): io_uring is blocked for it.
fn scratch_ring_ops() -> Result<Option<Supported>, ()> {
    // `struct io_uring_params` (120 bytes), all zero: no flags.
    let mut params = [0u64; 15];
    // SAFETY: `params` is as large as, and as aligned as, the
    // `io_uring_params` the kernel reads and writes; the descriptor
    // returned is closed below.
    let fd = unsafe {
        libc::syscall(
            libc::SYS_io_uring_setup,
            1u32,
            params.as_mut_ptr().cast::<libc::c_void>(),
        )
    };
    if fd < 0 {
        return match io::Error::last_os_error().raw_os_error() {
            Some(libc::EPERM | libc::EACCES | libc::ENOSYS) => Err(()),
            _ => Ok(None),
        };
    }
    let fd = fd as RawFd;
    let supported = probe_ops(fd);
    // SAFETY: the ring descriptor is ours, and closed once.
    unsafe { libc::close(fd) };
    Ok(supported)
}

/// Applies per-task restrictions (and, for read-only access, the open
/// filters) to the calling thread. Returns the report lines for
/// [`Layer::TaskRestrictions`] and [`Layer::OpenFilters`].
pub(crate) fn apply_thread(files: FileAccess) -> Vec<LayerReport> {
    let filters = |task: &LayerReport| match task.state {
        _ if files != FileAccess::ReadOnly => line(
            Layer::OpenFilters,
            LayerState::NotNeeded,
            "only FileAccess::ReadOnly needs open filters",
        ),
        LayerState::Applied => open_filters(-1, "new rings"),
        _ => line(
            Layer::OpenFilters,
            LayerState::Unavailable,
            "they come with per-task restrictions",
        ),
    };
    let task = match task_support() {
        TaskSupport::No(e) => line(
            Layer::TaskRestrictions,
            LayerState::Unavailable,
            format!(
                "Linux {} has no per-task io_uring restrictions (Linux 7.0 has them; the \
                 kernel said {}): rings created after start are not restricted",
                kernel_release(),
                errno_name(e)
            ),
        ),
        TaskSupport::Blocked => {
            return vec![
                line(
                    Layer::TaskRestrictions,
                    LayerState::NotNeeded,
                    "io_uring is blocked for this process (a seccomp profile or \
                     kernel.io_uring_disabled): no ring can be created to restrict",
                ),
                line(
                    Layer::OpenFilters,
                    LayerState::NotNeeded,
                    "io_uring is blocked for this process",
                ),
            ];
        }
        TaskSupport::AlreadyRestricted => {
            return vec![
                line(
                    Layer::TaskRestrictions,
                    LayerState::Applied,
                    "the thread already has per-task restrictions, from the thread that \
                     started it",
                ),
                line(
                    Layer::OpenFilters,
                    LayerState::Applied,
                    "kept from the thread that started it",
                ),
            ];
        }
        TaskSupport::Yes(supported) => restrict_task(files, supported.as_ref()),
    };
    let filters = filters(&task);
    vec![task, filters]
}

fn restrict_task(files: FileAccess, supported: Option<&Supported>) -> LayerReport {
    // SAFETY: plain prctl with integer arguments.
    if unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) } != 0 {
        return line(
            Layer::TaskRestrictions,
            LayerState::Unavailable,
            format!(
                "PR_SET_NO_NEW_PRIVS failed ({}), which per-task restrictions need",
                io::Error::last_os_error()
            ),
        );
    }
    let res = restrictions(files, TASK_REGISTER_OPS, supported);
    let mut task = TaskRestriction {
        flags: 0,
        nr_res: res.len() as u16,
        resv: [0; 3],
        restrictions: [Restriction::default(); MAX_RESTRICTIONS],
    };
    task.restrictions[..res.len()].copy_from_slice(&res);
    match register(
        -1,
        REGISTER_RESTRICTIONS,
        (&task as *const TaskRestriction).cast(),
        1,
    ) {
        Ok(_) | Err(libc::EPERM) => line(
            Layer::TaskRestrictions,
            LayerState::Applied,
            format!(
                "every ring this thread or a thread it starts creates from now on runs only \
                 the {} operations giong submits for FileAccess::{files:?} \
                 (no_new_privs is set)",
                sqe_ops(&res)
            ),
        ),
        Err(e) => line(
            Layer::TaskRestrictions,
            LayerState::Unavailable,
            format!(
                "Linux {} refused the per-task restrictions ({})",
                kernel_release(),
                errno_name(e)
            ),
        ),
    }
}

/// `IO_URING_BPF_CMD_FILTER`.
const BPF_CMD_FILTER: u16 = 1;
/// `IO_URING_BPF_FILTER_SZ_STRICT`: fail where the kernel does not fill in
/// the open's flags for the filter, rather than run it on zeros.
const BPF_FILTER_SZ_STRICT: u32 = 2;
/// The size of the context's `open` member: flags, mode, resolve.
const OPEN_PDU_SIZE: u8 = 24;
/// Where the low 32 bits of `open.flags` are in `struct io_uring_bpf_ctx`:
/// the union starts at byte 16, and a filter loads aligned 32-bit words in
/// the machine's byte order.
const OPEN_FLAGS_LOW: u32 = if cfg!(target_endian = "little") {
    16
} else {
    20
};

/// `struct io_uring_bpf_filter`.
#[repr(C)]
#[derive(Default)]
struct BpfFilter {
    opcode: u32,
    flags: u32,
    filter_len: u32,
    pdu_size: u8,
    resv: [u8; 3],
    filter_ptr: u64,
    resv2: [u64; 5],
}

/// `struct io_uring_bpf`.
#[repr(C)]
#[derive(Default)]
struct IoUringBpf {
    cmd_type: u16,
    cmd_flags: u16,
    resv: u32,
    filter: BpfFilter,
}

const _: () = assert!(std::mem::size_of::<IoUringBpf>() == 72);

/// `BPF_LD | BPF_W | BPF_ABS` (`BPF_LD` and `BPF_W` are 0).
const BPF_LD_W_ABS: u16 = 0x20;
const BPF_JMP_JSET_K: u16 = 0x05 | 0x40; // BPF_JMP | BPF_JSET | BPF_K
const BPF_RET_K: u16 = 0x06; // BPF_RET | BPF_K

/// The filter for an open: refused (0) when any of
/// [`write_open_flags`](super::write_open_flags) is set, allowed (1)
/// otherwise.
fn read_only_open_program() -> [libc::sock_filter; 4] {
    let insn = |code, jt, jf, k| libc::sock_filter { code, jt, jf, k };
    [
        insn(BPF_LD_W_ABS, 0, 0, OPEN_FLAGS_LOW),
        insn(BPF_JMP_JSET_K, 1, 0, super::write_open_flags() as u32),
        insn(BPF_RET_K, 0, 0, 1),
        insn(BPF_RET_K, 0, 0, 0),
    ]
}

/// Registers the read-only open filter for `openat` and `openat2` on the
/// ring `fd`, or on the calling task when `fd` is -1. `place` names it in
/// the report.
fn open_filters(fd: RawFd, place: &str) -> LayerReport {
    let program = read_only_open_program();
    for op in [opcode::OpenAt::CODE, opcode::OpenAt2::CODE] {
        let mut arg = IoUringBpf {
            cmd_type: BPF_CMD_FILTER,
            filter: BpfFilter {
                opcode: op as u32,
                flags: BPF_FILTER_SZ_STRICT,
                filter_len: program.len() as u32,
                pdu_size: OPEN_PDU_SIZE,
                filter_ptr: program.as_ptr() as u64,
                ..BpfFilter::default()
            },
            ..IoUringBpf::default()
        };
        if let Err(e) = register(
            fd,
            REGISTER_BPF_FILTER,
            (&mut arg as *mut IoUringBpf).cast(),
            1,
        ) {
            let why = match e {
                libc::EMSGSIZE => "it does not give filters the flags of an open".to_string(),
                e => format!("it has no io_uring BPF filters ({})", errno_name(e)),
            };
            return line(
                Layer::OpenFilters,
                LayerState::Unavailable,
                format!(
                    "Linux {} cannot refuse opens for writing on {place}: {why} (Linux 7.0 \
                     with CONFIG_IO_URING_BPF can)",
                    kernel_release()
                ),
            );
        }
    }
    line(
        Layer::OpenFilters,
        LayerState::Applied,
        format!("opens for writing, creating or truncating are refused on {place}"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn restrictions_fit_the_task_struct() {
        for files in [
            FileAccess::None,
            FileAccess::ReadOnly,
            FileAccess::ReadWrite,
        ] {
            assert!(restrictions(files, TASK_REGISTER_OPS, None).len() <= MAX_RESTRICTIONS);
        }
    }

    #[test]
    fn access_levels_add_path_operations() {
        let none = allowed_ops(FileAccess::None);
        let read = allowed_ops(FileAccess::ReadOnly);
        let write = allowed_ops(FileAccess::ReadWrite);
        assert!(!none.contains(&opcode::OpenAt::CODE));
        assert!(read.contains(&opcode::OpenAt::CODE));
        assert!(!read.contains(&opcode::UnlinkAt::CODE));
        assert!(write.contains(&opcode::RenameAt::CODE));
        assert!(none.contains(&opcode::Send::CODE));
    }

    #[test]
    fn the_open_filter_tests_the_write_flags() {
        let p = read_only_open_program();
        // A tiny interpreter for the four instructions, on a context whose
        // open flags are `flags`.
        let run = |flags: u32| {
            let mut ctx = [0u8; 64];
            ctx[16..24].copy_from_slice(&(flags as u64).to_ne_bytes());
            let mut a = 0u32;
            let mut pc = 0usize;
            loop {
                let i = p[pc];
                match i.code {
                    BPF_LD_W_ABS => {
                        let k = i.k as usize;
                        a = u32::from_ne_bytes(ctx[k..k + 4].try_into().unwrap());
                        pc += 1;
                    }
                    BPF_JMP_JSET_K => {
                        pc += 1 + if a & i.k != 0 { i.jt } else { i.jf } as usize;
                    }
                    BPF_RET_K => return i.k,
                    c => panic!("unexpected instruction {c:#x}"),
                }
            }
        };
        assert_eq!(run(libc::O_RDONLY as u32), 1);
        assert_eq!(
            run((libc::O_RDONLY | libc::O_CLOEXEC | libc::O_DIRECTORY) as u32),
            1
        );
        assert_eq!(run(libc::O_WRONLY as u32), 0);
        assert_eq!(run(libc::O_RDWR as u32), 0);
        assert_eq!(run((libc::O_RDONLY | libc::O_CREAT) as u32), 0);
        assert_eq!(run((libc::O_RDONLY | libc::O_TRUNC) as u32), 0);
        assert_eq!(run((libc::O_RDWR | libc::O_TMPFILE) as u32), 0);
    }
}
