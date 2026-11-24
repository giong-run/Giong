//! The parent of a process on Windows, from a Toolhelp32 snapshot.
//!
//! `giong dev status` and `stop` trust a service pid from `.giong/dev.json`
//! only when the verified runner is its parent; Windows offers no command
//! that tells a parent quickly, so this asks the system directly (#385).
//!
//! With `noinherit`, one of the two modules of giong-cli that use `unsafe`;
//! `[lints.rust] unsafe_code = "deny"` keeps it to them.
#![allow(unsafe_code)]

use std::ffi::c_void;

const TH32CS_SNAPPROCESS: u32 = 0x0000_0002;
const INVALID_HANDLE_VALUE: *mut c_void = -1isize as *mut c_void;

/// PROCESSENTRY32W, laid out as in tlhelp32.h.
#[repr(C)]
struct ProcessEntry {
    size: u32,
    usage: u32,
    process_id: u32,
    default_heap_id: usize,
    module_id: u32,
    threads: u32,
    parent_process_id: u32,
    priority_class_base: i32,
    flags: u32,
    exe_file: [u16; 260],
}

// The size the system checks in `size`: 568 bytes on 64-bit Windows, 556 on
// 32-bit.
const _: () = assert!(
    std::mem::size_of::<ProcessEntry>()
        == if cfg!(target_pointer_width = "64") {
            568
        } else {
            556
        }
);

#[link(name = "kernel32")]
extern "system" {
    fn CreateToolhelp32Snapshot(flags: u32, process_id: u32) -> *mut c_void;
    fn Process32FirstW(snapshot: *mut c_void, entry: *mut ProcessEntry) -> i32;
    fn Process32NextW(snapshot: *mut c_void, entry: *mut ProcessEntry) -> i32;
    fn CloseHandle(handle: *mut c_void) -> i32;
}

/// The parent of `pid`, or `None` when `pid` does not run or the system
/// does not say.
pub fn parent(pid: u64) -> Option<u64> {
    let pid = u32::try_from(pid).ok()?;
    // SAFETY: CreateToolhelp32Snapshot takes no pointers; the handle it
    // returns is checked against INVALID_HANDLE_VALUE (and null) before use
    // and closed exactly once below.
    let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
    if snapshot.is_null() || snapshot == INVALID_HANDLE_VALUE {
        return None;
    }
    let mut entry = ProcessEntry {
        size: std::mem::size_of::<ProcessEntry>() as u32,
        usage: 0,
        process_id: 0,
        default_heap_id: 0,
        module_id: 0,
        threads: 0,
        parent_process_id: 0,
        priority_class_base: 0,
        flags: 0,
        exe_file: [0; 260],
    };
    let mut found = None;
    // SAFETY: `snapshot` is a valid snapshot handle and `entry` a live,
    // writable PROCESSENTRY32W whose `size` field holds its size, as the
    // functions require; they write only within it.
    let mut more = unsafe { Process32FirstW(snapshot, &mut entry) } != 0;
    while more {
        if entry.process_id == pid {
            found = Some(u64::from(entry.parent_process_id));
            break;
        }
        // SAFETY: as for Process32FirstW above.
        more = unsafe { Process32NextW(snapshot, &mut entry) } != 0;
    }
    // SAFETY: `snapshot` is valid and not used after this.
    unsafe {
        CloseHandle(snapshot);
    }
    found
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_child_has_this_process_as_its_parent() {
        let mut child = std::process::Command::new("cmd")
            .args(["/C", "ping -n 3 127.0.0.1 > nul"])
            .spawn()
            .unwrap();
        let parent = super::parent(u64::from(child.id()));
        let _ = child.kill();
        let _ = child.wait();
        assert_eq!(parent, Some(u64::from(std::process::id())));
        assert_eq!(super::parent(u64::from(u32::MAX - 3)), None);
    }
}
