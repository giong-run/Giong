//! Keeps this process's standard handles out of the processes it starts
//! (Windows).
//!
//! std starts every child with `bInheritHandles = TRUE`, so the child, and
//! its children, also receive each inheritable handle this process holds. A
//! caller that reads giong through a pipe hands giong an inheritable write
//! end as stdout, so anything giong starts that outlives the command would
//! hold the caller's pipe open: the `giong dev --background` runner (#378),
//! or the grandchild of a tool giong gave up on after its timeout (#382).
//! The caller would then wait for those instead of for giong. Clearing
//! `HANDLE_FLAG_INHERIT` on giong's own standard handles once, first thing in
//! `main`, keeps them in this process. Children given `Stdio::inherit()` are
//! unaffected: std duplicates the handle into an inheritable one for them.
//!
//! With `winproc`, one of the two modules of giong-cli that use `unsafe`;
//! `[lints.rust] unsafe_code = "deny"` keeps it to them.
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
