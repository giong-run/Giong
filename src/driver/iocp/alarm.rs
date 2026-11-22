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
//! Arming costs three system calls, so a wait for the same deadline as the
//! last one, whose packet has not come yet, keeps the alarm as it is: a
//! busy server parks again and again before its next I/O timeout.
//!
//! The calls live in ntdll and are looked up when the driver starts. Where
//! one is missing (Wine, Windows before 10 1803 for the timer flag), the
//! driver keeps the millisecond timeout alone.

use std::cell::Cell;
use std::ffi::c_void;
use std::mem;
use std::ptr;
use std::time::{Duration, Instant};

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

/// The completion key of the alarm's packet; every other packet has 0.
pub(super) const KEY: usize = 1;

/// Deadlines this close are the same deadline: the caller passes the time
/// left, so each wait for one timer computes it again a few microseconds
/// apart. An alarm kept for a deadline this much earlier or later rings
/// at most this much off.
const SAME_DEADLINE: Duration = Duration::from_micros(50);

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
    /// When the armed alarm rings; `None` when its packet has come (see
    /// [`Alarm::rang`]) or it is not armed.
    armed: Cell<Option<Instant>>,
    /// How many times the timer was set.
    #[cfg(test)]
    arms: Cell<u32>,
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
            armed: Cell::new(None),
            #[cfg(test)]
            arms: Cell::new(0),
        })
    }

    /// Has `port` get a packet with no operation once `after` has passed,
    /// replacing what an earlier `set` asked for.
    pub(super) fn set(&self, port: HANDLE, after: Duration) -> Set {
        let now = Instant::now();
        let at = now.checked_add(after);
        if let (Some(armed), Some(at)) = (self.armed.get(), at) {
            if armed.max(at) - armed.min(at) <= SAME_DEADLINE {
                return if armed > now { Set::Armed } else { Set::No };
            }
        }
        self.armed.set(None);
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
        #[cfg(test)]
        self.arms.set(self.arms.get() + 1);
        let mut signaled = 0u8;
        // SAFETY: our packet and timer, and the caller's open port. The
        // packet carries no `OVERLAPPED` (a null context), which the driver
        // takes for a wake-up, and the key [`KEY`].
        let status = unsafe {
            (self.associate)(
                self.packet,
                port,
                self.timer,
                KEY as *const c_void,
                ptr::null(),
                0,
                0,
                &mut signaled,
            )
        };
        match (status, signaled) {
            (STATUS_SUCCESS, 0) => {
                self.armed.set(at);
                Set::Armed
            }
            (STATUS_SUCCESS, _) => Set::Expired,
            _ => Set::No,
        }
    }
}

impl Alarm {
    /// The port reported the alarm's packet: the next `set` arms again.
    pub(super) fn rang(&self) {
        self.armed.set(None);
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

#[cfg(test)]
mod tests {
    use super::*;
    use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
    use windows_sys::Win32::System::IO::{
        CreateIoCompletionPort, GetQueuedCompletionStatusEx, OVERLAPPED_ENTRY,
    };

    /// Waits up to `ms` for one packet; its key, or `None`.
    fn next_key(port: HANDLE, ms: u32) -> Option<usize> {
        // SAFETY: all-zero is a valid entry.
        let mut entry: OVERLAPPED_ENTRY = unsafe { mem::zeroed() };
        let mut n = 0u32;
        // SAFETY: our port; one entry of room.
        let ok = unsafe { GetQueuedCompletionStatusEx(port, &mut entry, 1, &mut n, ms, 0) };
        (ok != 0 && n == 1).then_some(entry.lpCompletionKey)
    }

    /// A wait for the deadline the alarm is already armed for keeps it; a
    /// wait for another deadline, or after the packet came, arms again.
    #[test]
    fn the_same_deadline_keeps_the_alarm() {
        let Some(alarm) = Alarm::new() else {
            eprintln!("skipped: this Windows has no high-resolution wait packets");
            return;
        };
        // SAFETY: a new port; closed below.
        let port = unsafe { CreateIoCompletionPort(INVALID_HANDLE_VALUE, ptr::null_mut(), 0, 1) };
        assert!(!port.is_null());
        let start = Instant::now();
        let deadline = start + Duration::from_millis(40);
        assert_eq!(alarm.set(port, deadline - Instant::now()), Set::Armed);
        assert_eq!(alarm.arms.get(), 1);
        for _ in 0..10 {
            assert_eq!(alarm.set(port, deadline - Instant::now()), Set::Armed);
        }
        assert_eq!(alarm.arms.get(), 1, "the same deadline set the timer again");
        assert_eq!(next_key(port, 1_000), Some(KEY));
        assert!(start.elapsed() >= Duration::from_millis(40));
        alarm.rang();
        // An earlier deadline arms again, and so does the old one once
        // its packet has come.
        assert_eq!(alarm.set(port, Duration::from_millis(30)), Set::Armed);
        assert_eq!(alarm.set(port, Duration::from_millis(10)), Set::Armed);
        assert_eq!(alarm.arms.get(), 3);
        let start = Instant::now();
        assert_eq!(next_key(port, 1_000), Some(KEY));
        let took = start.elapsed();
        assert!(took < Duration::from_millis(25), "rang after {took:?}");
        // The replaced alarm's packet never comes.
        assert_eq!(next_key(port, 100), None);
        // SAFETY: our port, closed once.
        unsafe { CloseHandle(port) };
    }
}
