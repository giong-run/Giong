//! A precise wake-up for the completion port at the next timer.
//!
//! `GetQueuedCompletionStatusEx` takes its timeout in milliseconds and ends
//! the wait on the system's clock tick, 15.6 ms by default: a 1 ms sleep
//! took 15.6 ms, and every timer fired up to a tick late. A
//! high-resolution waitable timer has no tick. Its expiry is queued to the
//! port as a packet with no operation (`NtAssociateWaitCompletionPacket`),
//! which ends the wait as any wake-up does. The approach Go takes since
//! 1.23; .NET's thread pool does the same.
//!
//! The calls live in ntdll and are looked up when the driver starts. Where
//! one is missing (Wine, Windows before 10 1803 for the timer flag), the
//! driver keeps the millisecond timeout alone.

use std::ffi::c_void;
use std::mem;
use std::ptr;
use std::time::Duration;

use windows_sys::Win32::Foundation::{CloseHandle, GENERIC_ALL, HANDLE};
use windows_sys::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress};
use windows_sys::Win32::System::Threading::{
    CreateWaitableTimerExW, SetWaitableTimer, CREATE_WAITABLE_TIMER_HIGH_RESOLUTION,
    TIMER_ALL_ACCESS,
};

const STATUS_SUCCESS: i32 = 0;
const STATUS_PENDING: i32 = 0x103;
const STATUS_CANCELLED: i32 = 0xC000_0120_u32 as i32;

type CreatePacket = unsafe extern "system" fn(*mut HANDLE, u32, *const c_void) -> i32;
type AssociatePacket = unsafe extern "system" fn(
    HANDLE,
    HANDLE,
    HANDLE,
    *const c_void,
    *const c_void,
    i32,
    usize,
    *mut u8,
) -> i32;
type CancelPacket = unsafe extern "system" fn(HANDLE, u8) -> i32;
/// What `GetProcAddress` returns.
type Export = unsafe extern "system" fn() -> isize;

/// What [`Alarm::set`] did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Set {
    /// The port gets a packet when the time has passed.
    Armed,
    /// The time passed while the alarm was being set: do not wait.
    Expired,
    /// Not set this time; the millisecond timeout alone ends the wait.
    No,
}

/// A high-resolution timer and the wait packet that reports it to a port.
pub(super) struct Alarm {
    timer: HANDLE,
    packet: HANDLE,
    associate: AssociatePacket,
    cancel: CancelPacket,
}

impl Alarm {
    /// `None` where this Windows lacks one of the parts.
    pub(super) fn new() -> Option<Self> {
        // SAFETY: looks up exports of ntdll, which every process has
        // loaded; each is transmuted to the signature ntdll gives it.
        let (create, associate, cancel) = unsafe {
            let ntdll = GetModuleHandleW(windows_sys::w!("ntdll.dll"));
            if ntdll.is_null() {
                return None;
            }
            let create = GetProcAddress(ntdll, c"NtCreateWaitCompletionPacket".as_ptr().cast())?;
            let associate =
                GetProcAddress(ntdll, c"NtAssociateWaitCompletionPacket".as_ptr().cast())?;
            let cancel = GetProcAddress(ntdll, c"NtCancelWaitCompletionPacket".as_ptr().cast())?;
            (
                mem::transmute::<Export, CreatePacket>(create),
                mem::transmute::<Export, AssociatePacket>(associate),
                mem::transmute::<Export, CancelPacket>(cancel),
            )
        };
        // SAFETY: creates an unnamed timer; the result is checked.
        let timer = unsafe {
            CreateWaitableTimerExW(
                ptr::null(),
                ptr::null(),
                CREATE_WAITABLE_TIMER_HIGH_RESOLUTION,
                TIMER_ALL_ACCESS,
            )
        };
        if timer.is_null() {
            return None;
        }
        let mut packet: HANDLE = ptr::null_mut();
        // SAFETY: creates a wait packet into a local; the status is checked.
        if unsafe { create(&mut packet, GENERIC_ALL, ptr::null()) } != STATUS_SUCCESS {
            // SAFETY: the timer made above, closed once.
            unsafe { CloseHandle(timer) };
            return None;
        }
        Some(Self {
            timer,
            packet,
            associate,
            cancel,
        })
    }

    /// Has `port` get a packet with no operation once `after` has passed,
    /// replacing what an earlier `set` asked for.
    pub(super) fn set(&self, port: HANDLE, after: Duration) -> Set {
        // The packet of an earlier set may still wait on the timer, or be
        // queued and not taken yet: both go. STATUS_CANCELLED says the
        // timer had fired and the association ended by itself.
        // SAFETY: our packet; `1` also removes a packet already queued.
        match unsafe { (self.cancel)(self.packet, 1) } {
            STATUS_SUCCESS | STATUS_CANCELLED => {}
            // The packet is being queued right now and cannot be taken
            // back: this wait goes without the alarm, and the packet only
            // ends a later wait early, which the event loop handles.
            STATUS_PENDING => return Set::No,
            _ => return Set::No,
        }
        // Relative due times are negative, in units of 100 ns; rounded up,
        // so the alarm never rings before the time.
        let units = after.as_nanos().div_ceil(100).clamp(1, i64::MAX as u128) as i64;
        let due = -units;
        // SAFETY: our timer; `due` is a local; no completion routine.
        if unsafe { SetWaitableTimer(self.timer, &due, 0, None, ptr::null(), 0) } == 0 {
            return Set::No;
        }
        let mut signaled = 0u8;
        // SAFETY: our packet and timer, and the caller's open port. The
        // packet carries no `OVERLAPPED` (a null context), which the driver
        // takes for a wake-up.
        let status = unsafe {
            (self.associate)(
                self.packet,
                port,
                self.timer,
                ptr::null(),
                ptr::null(),
                0,
                0,
                &mut signaled,
            )
        };
        match (status, signaled) {
            (STATUS_SUCCESS, 0) => Set::Armed,
            (STATUS_SUCCESS, _) => Set::Expired,
            _ => Set::No,
        }
    }
}

impl Drop for Alarm {
    fn drop(&mut self) {
        // SAFETY: our packet and timer, each closed once; the cancel takes
        // a queued packet off the port first.
        unsafe {
            (self.cancel)(self.packet, 1);
            CloseHandle(self.packet);
            CloseHandle(self.timer);
        }
    }
}
