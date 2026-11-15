//! Valgrind client requests for callgrind, without a dependency: the same
//! instruction sequence as the `VALGRIND_DO_CLIENT_REQUEST` macro of
//! valgrind.h on x86-64. Outside valgrind the sequence does nothing and
//! returns the default value.

/// `VG_USERREQ_TOOL_BASE('C', 'T')`, callgrind's first request
/// (callgrind.h): DUMP_STATS, ZERO_STATS, TOGGLE_COLLECT, DUMP_STATS_AT, ...
const CALLGRIND_BASE: usize = ((b'C' as usize) << 24) | ((b'T' as usize) << 16);
const ZERO_STATS: usize = CALLGRIND_BASE + 1;
const DUMP_STATS_AT: usize = CALLGRIND_BASE + 3;

#[cfg(target_arch = "x86_64")]
fn request(args: [usize; 6]) -> usize {
    let mut result = 0usize;
    // SAFETY: the four rotations of rdi add up to 128 bits, a no-op, and
    // `xchg rbx, rbx` is a no-op: natively nothing changes but the flags.
    // Under valgrind the sequence is a client request: valgrind reads the
    // six words at rax (alive for the whole statement) and writes its
    // answer to rdx. `args` is not written to.
    unsafe {
        std::arch::asm!(
            "rol rdi, 3",
            "rol rdi, 13",
            "rol rdi, 61",
            "rol rdi, 51",
            "xchg rbx, rbx",
            inout("rdx") result,
            in("rax") args.as_ptr(),
            out("rdi") _,
            options(nostack),
        );
    }
    result
}

#[cfg(not(target_arch = "x86_64"))]
fn request(_args: [usize; 6]) -> usize {
    0
}

/// Zeroes callgrind's counters (of every thread).
pub fn zero_stats() {
    request([ZERO_STATS, 0, 0, 0, 0, 0]);
}

/// Writes callgrind's counters to a new dump file, its `desc:` line naming
/// `name`, and zeroes them.
pub fn dump_stats_at(name: &str) {
    let c = std::ffi::CString::new(name).expect("a name without NUL");
    request([DUMP_STATS_AT, c.as_ptr() as usize, 0, 0, 0, 0]);
}
