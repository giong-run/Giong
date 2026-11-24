//! Keeps this process's standard handles out of a detached runner (Windows).
//!
//! std starts every child with `bInheritHandles = TRUE`, so the child also
//! receives each inheritable handle this process holds. A caller that reads
//! `giong dev --background` through a pipe hands giong an inheritable write
//! end as stdout: the runner, which outlives the command, would hold it too,
//! and the caller would wait for the runner to exit instead of for the
//! command (#378). Clearing `HANDLE_FLAG_INHERIT` on giong's own standard
//! handles just before the runner is spawned keeps them in this process.
//! Children given `Stdio::inherit()` later are unaffected: std duplicates the
//! handle into an inheritable one for them.
//!
//! The only `unsafe` in giong-cli; `[lints.rust] unsafe_code = "deny"` keeps
//! it to this module.
#![allow(unsafe_code)]

use std::ffi::c_void;
use std::os::windows::io::AsRawHandle;

const HANDLE_FLAG_INHERIT: u32 = 0x0000_0001;

#[link(name = "kernel32")]
extern "system" {
    fn SetHandleInformation(handle: *mut c_void, mask: u32, flags: u32) -> i32;
}

/// Makes stdin, stdout and stderr of this process non-inheritable. A handle
/// that is missing or not a real handle is left alone.
pub fn keep_std_handles() {
    let handles = [
        std::io::stdin().as_raw_handle(),
        std::io::stdout().as_raw_handle(),
        std::io::stderr().as_raw_handle(),
    ];
    for handle in handles {
        if handle.is_null() {
            continue;
        }
        // SAFETY: `handle` is this process's own standard handle, borrowed
        // from std and still open (std never closes them). SetHandleInformation
        // only changes a flag of that handle-table entry; on a handle that is
        // not valid (INVALID_HANDLE_VALUE, a detached process) it fails and
        // returns 0, which is ignored: there is then nothing to keep out.
        unsafe {
            SetHandleInformation(handle, HANDLE_FLAG_INHERIT, 0);
        }
    }
}
