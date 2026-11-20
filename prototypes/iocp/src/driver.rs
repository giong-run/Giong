//! The completion-port driver.
//!
//! One port per worker. Every socket and file a worker uses is associated
//! with its port once ([`Driver::register`]); from then on each operation
//! on it is an overlapped call whose completion the port reports.
//!
//! # What stays alive, and until when
//!
//! Each operation in flight has a `Record` on the heap. The kernel holds
//! its address (it begins with the `OVERLAPPED`), so a record is freed in
//! exactly two places: when the call that would have started the operation
//! says nothing is in flight, and when the port reports its completion.
//! Never anywhere else, and in particular not when its future is dropped.
//!
//! Dropping a future ([`Driver::cancel_op`]) asks the kernel to cancel
//! (`CancelIoEx`) and moves the operation's resources (its buffers, an
//! accept's socket) into the record. Cancellation on Windows is a request:
//! the completion still arrives later, with `ERROR_OPERATION_ABORTED` or
//! with the result of an operation that won the race. Only then are the
//! record and the resources released. That is the contract the io_uring
//! driver keeps, with the same shape.
//!
//! # Results
//!
//! A result is the number of bytes transferred, or the negated Windows
//! error code (`-(WSAECONNRESET as i32)`), which
//! `std::io::Error::from_raw_os_error(-res)` turns into the right
//! `ErrorKind`.

use std::any::Any;
use std::ffi::c_void;
use std::io;
use std::mem;
use std::ptr;
use std::sync::Arc;
use std::task::{Poll, Waker};
use std::time::Duration;

use windows_sys::Win32::Foundation::{
    CloseHandle, GetLastError, ERROR_HANDLE_EOF, ERROR_IO_PENDING, ERROR_NOT_FOUND, HANDLE,
    INVALID_HANDLE_VALUE, WAIT_TIMEOUT,
};
use windows_sys::Win32::Networking::WinSock::{
    setsockopt, TransmitFile, WSAGetLastError, WSAGetOverlappedResult, WSAIoctl, WSARecv,
    WSARecvFrom, WSASend, WSASendTo, LPFN_ACCEPTEX, LPFN_CONNECTEX,
    SIO_GET_EXTENSION_FUNCTION_POINTER, SOCKADDR, SOCKET, SOL_SOCKET, SO_UPDATE_ACCEPT_CONTEXT,
    SO_UPDATE_CONNECT_CONTEXT, WSABUF, WSAID_ACCEPTEX, WSAID_CONNECTEX,
};
use windows_sys::Win32::Storage::FileSystem::{
    ReadFile, SetFileCompletionNotificationModes, WriteFile,
};
use windows_sys::Win32::System::WindowsProgramming::{
    FILE_SKIP_COMPLETION_PORT_ON_SUCCESS, FILE_SKIP_SET_EVENT_ON_HANDLE,
};
use windows_sys::Win32::System::IO::{
    CancelIoEx, CreateIoCompletionPort, GetOverlappedResult, GetQueuedCompletionStatusEx,
    PostQueuedCompletionStatus, OVERLAPPED, OVERLAPPED_ENTRY,
};

/// Bytes `AcceptEx` needs for each of the two addresses it writes: the
/// largest address plus 16, as its documentation requires.
pub const ACCEPT_ADDR_LEN: usize =
    mem::size_of::<windows_sys::Win32::Networking::WinSock::SOCKADDR_STORAGE>() + 16;

/// How many completions one call to the kernel collects.
const BATCH: usize = 256;

/// A single-shot operation. Every pointer must stay valid, at the same
/// address, until the operation's completion has been collected or, if its
/// future is dropped, until the driver lets go of the resources handed to
/// [`Driver::cancel_op`].
#[derive(Clone, Copy, Debug)]
pub enum Io {
    /// `AcceptEx` on `listener` into `socket`, a socket of the listener's
    /// family created ahead, not bound and not connected. `addrs` has room
    /// for `2 * ACCEPT_ADDR_LEN` bytes. Result 0; `socket` is then a
    /// connected socket like any other (the driver has run
    /// `SO_UPDATE_ACCEPT_CONTEXT`).
    Accept {
        listener: SOCKET,
        socket: SOCKET,
        addrs: *mut u8,
    },
    /// `ConnectEx`. The socket must be bound already (to the wildcard
    /// address for an ordinary connect). Result 0.
    Connect {
        socket: SOCKET,
        addr: *const SOCKADDR,
        len: i32,
    },
    /// Receive up to `len` bytes. With `len == 0` it completes, with result
    /// 0, when the socket has data or was closed by the peer, and holds no
    /// buffer while it waits.
    Recv {
        socket: SOCKET,
        buf: *mut u8,
        len: u32,
    },
    Send {
        socket: SOCKET,
        buf: *const u8,
        len: u32,
    },
    /// Send several buffers as one write (an HTTP head and its body).
    /// `bufs` points at `count` entries, kept with the buffers they name.
    SendV {
        socket: SOCKET,
        bufs: *const WSABUF,
        count: u32,
    },
    /// Send `len` bytes of `file` from `offset` without copying them
    /// through the process (`TransmitFile`). `len` is at most 2^31 - 1 and
    /// not 0, which Windows reads as "to the end of the file". The file
    /// handle need not be registered with the driver.
    SendFile {
        socket: SOCKET,
        file: HANDLE,
        offset: u64,
        len: u32,
    },
    /// Receive one datagram and the address it came from. `addr_len` holds
    /// the size of `addr` going in and the address's length coming out.
    RecvFrom {
        socket: SOCKET,
        buf: *mut u8,
        len: u32,
        addr: *mut SOCKADDR,
        addr_len: *mut i32,
    },
    SendTo {
        socket: SOCKET,
        buf: *const u8,
        len: u32,
        addr: *const SOCKADDR,
        addr_len: i32,
    },
    /// Read from a file at `offset`. Result 0 at the end of the file.
    Read {
        file: HANDLE,
        buf: *mut u8,
        len: u32,
        offset: u64,
    },
    Write {
        file: HANDLE,
        buf: *const u8,
        len: u32,
        offset: u64,
    },
}

impl Io {
    /// The handle the operation runs on, and whether it is a socket.
    fn handle(&self) -> (HANDLE, bool) {
        match *self {
            Io::Accept { listener, .. } => (listener as HANDLE, true),
            Io::Connect { socket, .. }
            | Io::Recv { socket, .. }
            | Io::Send { socket, .. }
            | Io::SendV { socket, .. }
            | Io::SendFile { socket, .. }
            | Io::RecvFrom { socket, .. }
            | Io::SendTo { socket, .. } => (socket as HANDLE, true),
            Io::Read { file, .. } | Io::Write { file, .. } => (file, false),
        }
    }
}

/// How [`Driver::start`] dealt with an operation.
#[derive(Debug)]
pub enum Start {
    /// Finished inside the call: the result. No completion will follow.
    Done(i32),
    /// In flight under this key: collect it with [`Driver::poll_op`].
    Submitted(usize),
}

/// How [`Driver::park`] enters the kernel.
#[derive(Clone, Copy, Debug)]
pub enum Park {
    /// Collect what has completed, without waiting.
    Poll,
    /// Wait for a completion or a ring of the bell, at most this long.
    Wait(Option<Duration>),
}

/// What a socket needs once its `AcceptEx` or `ConnectEx` has succeeded,
/// before it behaves like a connected socket (`shutdown`, `getpeername`).
#[derive(Clone, Copy)]
enum After {
    Nothing,
    Accepted { listener: SOCKET, socket: SOCKET },
    Connected { socket: SOCKET },
}

enum State {
    /// In flight; its future waits.
    Waiting(Waker),
    /// Completed; the future has not collected the result yet.
    Done(i32),
    /// The future is gone. The resources the kernel may still touch wait
    /// here for the completion.
    Orphan(#[allow(dead_code)] Option<Box<dyn Any>>),
}

/// One operation in flight. The kernel holds a pointer to `overlapped`,
/// which is the record's address.
#[repr(C)]
struct Record {
    overlapped: OVERLAPPED,
    handle: HANDLE,
    socket: bool,
    after: After,
    state: State,
}

/// The completion port. Closed when the driver and every [`Bell`] are gone.
struct Port(HANDLE);

// SAFETY: a completion port handle may be used from any thread; posting to
// it and waiting on it are thread safe by the port's contract.
unsafe impl Send for Port {}
// SAFETY: as above.
unsafe impl Sync for Port {}

impl Drop for Port {
    fn drop(&mut self) {
        // SAFETY: the handle is a port this value owns, closed once.
        unsafe { CloseHandle(self.0) };
    }
}

/// Wakes a worker that waits in [`Driver::park`], from any thread. On
/// Windows the bell is the port itself: ringing posts a packet to it.
#[derive(Clone)]
pub struct Bell(Arc<Port>);

impl Bell {
    /// Make the worker's next (or current) `park` return. Rings are not
    /// counted: several before one `park` wake it once or several times.
    pub fn ring(&self) {
        // SAFETY: the port is open for as long as `self` holds it. A null
        // OVERLAPPED marks the packet as a ring.
        let ok = unsafe { PostQueuedCompletionStatus(self.0 .0, 0, 0, ptr::null()) };
        // It fails only when the kernel cannot allocate the packet; the
        // worker then wakes at its next timer or completion.
        let _ = ok;
    }
}

/// What one [`Driver::park`] did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Parked {
    /// Operations whose completion was collected.
    pub completions: usize,
    /// The bell rang.
    pub rung: bool,
}

pub struct Driver {
    port: Arc<Port>,
    /// Operations whose future is still there, by key.
    ops: Vec<*mut Record>,
    free: Vec<usize>,
    /// Records the kernel holds, orphans included.
    inflight: usize,
    events: Vec<OVERLAPPED_ENTRY>,
    /// Whether a call that succeeds at once reports no completion.
    skip_on_success: bool,
    accept_ex: LPFN_ACCEPTEX,
    connect_ex: LPFN_CONNECTEX,
}

fn last_error() -> i32 {
    // SAFETY: reads the calling thread's last error code.
    unsafe { GetLastError() as i32 }
}

fn wsa_error() -> i32 {
    // SAFETY: reads the calling thread's last socket error code.
    unsafe { WSAGetLastError() }
}

impl Driver {
    pub fn new() -> io::Result<Self> {
        Self::with_inline_completions(true)
    }

    /// With `inline == false` every operation reports through the port,
    /// even one that finished inside its call. Slower; it exists so that
    /// tests run both paths.
    pub fn with_inline_completions(inline: bool) -> io::Result<Self> {
        // SAFETY: creates a new port; no handle is associated yet. One
        // thread waits on it, hence a concurrency of 1.
        let port = unsafe { CreateIoCompletionPort(INVALID_HANDLE_VALUE, ptr::null_mut(), 0, 1) };
        if port.is_null() {
            return Err(io::Error::last_os_error());
        }
        Ok(Self {
            port: Arc::new(Port(port)),
            ops: Vec::new(),
            free: Vec::new(),
            inflight: 0,
            // SAFETY: OVERLAPPED_ENTRY is plain data; all-zero is valid.
            events: vec![unsafe { mem::zeroed() }; BATCH],
            skip_on_success: inline,
            accept_ex: None,
            connect_ex: None,
        })
    }

    pub fn bell(&self) -> Bell {
        Bell(self.port.clone())
    }

    /// Operations the kernel has not reported yet, including those whose
    /// future was dropped.
    pub fn inflight(&self) -> usize {
        self.inflight
    }

    /// Operations in flight or completed whose future is still there.
    pub fn tracked_ops(&self) -> usize {
        self.ops.len() - self.free.len()
    }

    /// Associate a socket with this worker's port. Once, before its first
    /// operation; a handle belongs to one port for its whole life.
    pub fn register_socket(&mut self, socket: SOCKET) -> io::Result<()> {
        // SAFETY: a socket is named by a number, not a pointer; one that
        // is not open makes the call fail.
        unsafe { self.register(socket as HANDLE) }
    }

    /// Associate a file opened with `FILE_FLAG_OVERLAPPED` with the port.
    ///
    /// # Safety
    ///
    /// `handle` is an open handle.
    pub unsafe fn register(&mut self, handle: HANDLE) -> io::Result<()> {
        // SAFETY: associates a handle the caller owns with our open port.
        let port = unsafe { CreateIoCompletionPort(handle, self.port.0, 0, 0) };
        if port.is_null() {
            return Err(io::Error::last_os_error());
        }
        let mut modes = FILE_SKIP_SET_EVENT_ON_HANDLE as u8;
        if self.skip_on_success {
            modes |= FILE_SKIP_COMPLETION_PORT_ON_SUCCESS as u8;
        }
        // SAFETY: sets notification modes on a handle the caller owns.
        if unsafe { SetFileCompletionNotificationModes(handle, modes) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    /// `AcceptEx` and `ConnectEx` are reached through a function pointer
    /// that the socket's provider hands out.
    fn extension<F>(socket: SOCKET, id: windows_sys::core::GUID) -> Result<Option<F>, i32> {
        debug_assert_eq!(mem::size_of::<Option<F>>(), mem::size_of::<usize>());
        let mut function: usize = 0;
        let mut bytes = 0u32;
        // SAFETY: a synchronous ioctl; input and output point at locals of
        // the sizes given.
        let rc = unsafe {
            WSAIoctl(
                socket,
                SIO_GET_EXTENSION_FUNCTION_POINTER,
                (&id as *const windows_sys::core::GUID).cast(),
                mem::size_of_val(&id) as u32,
                (&mut function as *mut usize).cast(),
                mem::size_of::<usize>() as u32,
                &mut bytes,
                ptr::null_mut(),
                None,
            )
        };
        if rc != 0 {
            return Err(-wsa_error());
        }
        // SAFETY: `Option<extern fn>` has the layout of a pointer, null
        // being `None`; the provider returned a function of this type for
        // this identifier.
        Ok(unsafe { mem::transmute_copy::<usize, Option<F>>(&function) })
    }

    /// Begin an operation.
    ///
    /// # Safety
    ///
    /// `io`'s handle is registered with this driver, and its pointers obey
    /// the rule on [`Io`].
    pub unsafe fn start(&mut self, io: &Io, waker: &Waker) -> Start {
        let (handle, socket) = io.handle();
        let mut after = After::Nothing;
        // SAFETY: OVERLAPPED is plain data; all-zero is its initial state.
        let mut overlapped: OVERLAPPED = unsafe { mem::zeroed() };
        if let Io::Read { offset, .. } | Io::Write { offset, .. } | Io::SendFile { offset, .. } =
            *io
        {
            overlapped.Anonymous.Anonymous.Offset = offset as u32;
            overlapped.Anonymous.Anonymous.OffsetHigh = (offset >> 32) as u32;
        }
        let record = Box::into_raw(Box::new(Record {
            overlapped,
            handle,
            socket,
            after,
            state: State::Waiting(waker.clone()),
        }));
        let ov = record.cast::<OVERLAPPED>();
        let mut transferred = 0u32;

        // Each arm leaves `Ok(())` for "the call succeeded at once" and
        // `Err(code)` otherwise, where ERROR_IO_PENDING means in flight.
        //
        // SAFETY: in every arm, the caller guarantees the handle and the
        // pointers; `ov` points at the record, which lives until its
        // completion.
        let outcome: Result<(), i32> = unsafe {
            match *io {
                Io::Accept {
                    listener,
                    socket: accepted,
                    addrs,
                } => {
                    after = After::Accepted {
                        listener,
                        socket: accepted,
                    };
                    match self.accept_ex_for(listener) {
                        Err(code) => Err(-code),
                        Ok(accept_ex) => {
                            let ok = accept_ex(
                                listener,
                                accepted,
                                addrs.cast(),
                                0,
                                ACCEPT_ADDR_LEN as u32,
                                ACCEPT_ADDR_LEN as u32,
                                &mut transferred,
                                ov,
                            );
                            if ok != 0 {
                                Ok(())
                            } else {
                                Err(wsa_error())
                            }
                        }
                    }
                }
                Io::Connect {
                    socket: s,
                    addr,
                    len,
                } => {
                    after = After::Connected { socket: s };
                    match self.connect_ex_for(s) {
                        Err(code) => Err(-code),
                        Ok(connect_ex) => {
                            let ok = connect_ex(s, addr, len, ptr::null(), 0, ptr::null_mut(), ov);
                            if ok != 0 {
                                Ok(())
                            } else {
                                Err(wsa_error())
                            }
                        }
                    }
                }
                Io::Recv {
                    socket: s,
                    buf,
                    len,
                } => {
                    let wsabuf = WSABUF { len, buf };
                    let mut flags = 0u32;
                    let rc = WSARecv(s, &wsabuf, 1, &mut transferred, &mut flags, ov, None);
                    if rc == 0 {
                        Ok(())
                    } else {
                        Err(wsa_error())
                    }
                }
                Io::Send {
                    socket: s,
                    buf,
                    len,
                } => {
                    let wsabuf = WSABUF {
                        len,
                        buf: buf.cast_mut(),
                    };
                    let rc = WSASend(s, &wsabuf, 1, &mut transferred, 0, ov, None);
                    if rc == 0 {
                        Ok(())
                    } else {
                        Err(wsa_error())
                    }
                }
                Io::SendV {
                    socket: s,
                    bufs,
                    count,
                } => {
                    let rc = WSASend(s, bufs, count, &mut transferred, 0, ov, None);
                    if rc == 0 {
                        Ok(())
                    } else {
                        Err(wsa_error())
                    }
                }
                Io::SendFile {
                    socket: s,
                    file,
                    len,
                    ..
                } => {
                    if TransmitFile(s, file, len, 0, ov, ptr::null(), 0) != 0 {
                        // TransmitFile has no out-parameter for the count:
                        // a call that finishes at once leaves it in the
                        // OVERLAPPED, as a completion would.
                        transferred = (*record).overlapped.InternalHigh as u32;
                        Ok(())
                    } else {
                        Err(wsa_error())
                    }
                }
                Io::RecvFrom {
                    socket: s,
                    buf,
                    len,
                    addr,
                    addr_len,
                } => {
                    let wsabuf = WSABUF { len, buf };
                    let mut flags = 0u32;
                    let rc = WSARecvFrom(
                        s,
                        &wsabuf,
                        1,
                        &mut transferred,
                        &mut flags,
                        addr,
                        addr_len,
                        ov,
                        None,
                    );
                    if rc == 0 {
                        Ok(())
                    } else {
                        Err(wsa_error())
                    }
                }
                Io::SendTo {
                    socket: s,
                    buf,
                    len,
                    addr,
                    addr_len,
                } => {
                    let wsabuf = WSABUF {
                        len,
                        buf: buf.cast_mut(),
                    };
                    let rc =
                        WSASendTo(s, &wsabuf, 1, &mut transferred, 0, addr, addr_len, ov, None);
                    if rc == 0 {
                        Ok(())
                    } else {
                        Err(wsa_error())
                    }
                }
                Io::Read { file, buf, len, .. } => {
                    if ReadFile(file, buf, len, &mut transferred, ov) != 0 {
                        Ok(())
                    } else {
                        Err(last_error())
                    }
                }
                Io::Write { file, buf, len, .. } => {
                    if WriteFile(file, buf, len, &mut transferred, ov) != 0 {
                        Ok(())
                    } else {
                        Err(last_error())
                    }
                }
            }
        };
        // SAFETY: the record is ours; the kernel only reads its OVERLAPPED.
        unsafe { (*record).after = after };

        let in_flight = match outcome {
            // Succeeded inside the call. The port reports it as well unless
            // the handle skips completions on success.
            Ok(()) => !self.skip_on_success,
            Err(code) => code == ERROR_IO_PENDING as i32,
        };
        if in_flight {
            self.inflight += 1;
            let key = match self.free.pop() {
                Some(key) => {
                    self.ops[key] = record;
                    key
                }
                None => {
                    self.ops.push(record);
                    self.ops.len() - 1
                }
            };
            return Start::Submitted(key);
        }
        // Nothing is in flight: no completion will name this record.
        // SAFETY: it came from `Box::into_raw` above and the kernel does
        // not hold it.
        drop(unsafe { Box::from_raw(record) });
        Start::Done(match outcome {
            Ok(()) => match Self::finish(after) {
                0 => transferred as i32,
                err => err,
            },
            // A code already negated came from looking the extension up.
            Err(code) if code < 0 => code,
            Err(code) if code == ERROR_HANDLE_EOF as i32 => 0,
            Err(code) => -code,
        })
    }

    fn accept_ex_for(
        &mut self,
        socket: SOCKET,
    ) -> Result<
        unsafe extern "system" fn(
            SOCKET,
            SOCKET,
            *mut c_void,
            u32,
            u32,
            u32,
            *mut u32,
            *mut OVERLAPPED,
        ) -> i32,
        i32,
    > {
        if self.accept_ex.is_none() {
            self.accept_ex = Self::extension(socket, WSAID_ACCEPTEX).map_err(|e| -e)?;
        }
        self.accept_ex
            .ok_or(windows_sys::Win32::Networking::WinSock::WSAEOPNOTSUPP)
    }

    fn connect_ex_for(
        &mut self,
        socket: SOCKET,
    ) -> Result<
        unsafe extern "system" fn(
            SOCKET,
            *const SOCKADDR,
            i32,
            *const c_void,
            u32,
            *mut u32,
            *mut OVERLAPPED,
        ) -> i32,
        i32,
    > {
        if self.connect_ex.is_none() {
            self.connect_ex = Self::extension(socket, WSAID_CONNECTEX).map_err(|e| -e)?;
        }
        self.connect_ex
            .ok_or(windows_sys::Win32::Networking::WinSock::WSAEOPNOTSUPP)
    }

    /// Run what a successful accept or connect still needs. 0, or the
    /// negated error.
    fn finish(after: After) -> i32 {
        let rc = match after {
            After::Nothing => 0,
            // SAFETY: both sockets are open (the operation just succeeded
            // on them); the option value is the listener's handle.
            After::Accepted { listener, socket } => unsafe {
                setsockopt(
                    socket,
                    SOL_SOCKET,
                    SO_UPDATE_ACCEPT_CONTEXT,
                    (&listener as *const SOCKET).cast(),
                    mem::size_of::<SOCKET>() as i32,
                )
            },
            // SAFETY: the socket is open; this option takes no value.
            After::Connected { socket } => unsafe {
                setsockopt(
                    socket,
                    SOL_SOCKET,
                    SO_UPDATE_CONNECT_CONTEXT,
                    ptr::null(),
                    0,
                )
            },
        };
        if rc == 0 {
            0
        } else {
            -wsa_error()
        }
    }

    /// The result of an operation, once the port has reported it.
    pub fn poll_op(&mut self, key: usize, waker: &Waker) -> Poll<i32> {
        let record = self.ops[key];
        // SAFETY: a key names a live record until `poll_op` returns Ready
        // or `cancel_op` is called for it, and the caller does neither
        // twice.
        let state = unsafe { &mut (*record).state };
        match state {
            State::Done(res) => {
                let res = *res;
                // SAFETY: completed, so the kernel no longer holds it, and
                // it was collected by `complete`, which keeps `Done`
                // records for exactly this call.
                drop(unsafe { Box::from_raw(record) });
                self.release(key);
                Poll::Ready(res)
            }
            State::Waiting(current) => {
                if !current.will_wake(waker) {
                    *current = waker.clone();
                }
                Poll::Pending
            }
            State::Orphan(_) => unreachable!("giong: an operation was polled after its drop"),
        }
    }

    /// The future of an operation was dropped before it collected the
    /// result. `data` is what the operation's pointers point into; the
    /// driver keeps it until the kernel has reported the operation. With
    /// `finish` the operation is left to run to its end.
    pub fn cancel_op(&mut self, key: usize, data: Option<Box<dyn Any>>, finish: bool) {
        let record = self.ops[key];
        self.release(key);
        // SAFETY: as in `poll_op`.
        let rec = unsafe { &mut *record };
        if let State::Done(_) = rec.state {
            // Already reported: nothing is in flight.
            // SAFETY: as in `poll_op`.
            drop(unsafe { Box::from_raw(record) });
            return;
        }
        rec.state = State::Orphan(data);
        if !finish {
            // SAFETY: cancels one operation by its OVERLAPPED, on the
            // handle it was started on. Should the handle be closed
            // already, the call fails and the completion comes anyway.
            let ok = unsafe { CancelIoEx(rec.handle, &rec.overlapped) };
            if ok == 0 {
                // ERROR_NOT_FOUND: it completed already and its packet is
                // queued. Any other failure also leaves the packet coming.
                debug_assert!(
                    matches!(last_error() as u32, ERROR_NOT_FOUND | 6),
                    "CancelIoEx: {}",
                    io::Error::last_os_error()
                );
            }
        }
    }

    fn release(&mut self, key: usize) {
        self.ops[key] = ptr::null_mut();
        self.free.push(key);
    }

    /// Enter the kernel once: collect completions, wake their futures, and
    /// free what dropped futures left behind.
    pub fn park(&mut self, mode: Park) -> Parked {
        let mut timeout = match mode {
            Park::Poll => 0,
            Park::Wait(None) => u32::MAX, // INFINITE
            Park::Wait(Some(d)) => {
                // Round up: waking early would spin until the deadline.
                let ms = d.as_nanos().div_ceil(1_000_000);
                ms.min((u32::MAX - 1) as u128) as u32
            }
        };
        let mut parked = Parked::default();
        loop {
            let mut count = 0u32;
            // SAFETY: `events` has BATCH entries for the kernel to fill.
            let ok = unsafe {
                GetQueuedCompletionStatusEx(
                    self.port.0,
                    self.events.as_mut_ptr(),
                    BATCH as u32,
                    &mut count,
                    timeout,
                    0,
                )
            };
            if ok == 0 {
                let err = io::Error::last_os_error();
                if err.raw_os_error() == Some(WAIT_TIMEOUT as i32) {
                    return parked;
                }
                panic!("giong: GetQueuedCompletionStatusEx failed: {err}");
            }
            for i in 0..count as usize {
                let entry = self.events[i];
                if entry.lpOverlapped.is_null() {
                    parked.rung = true;
                } else {
                    self.complete(entry);
                    parked.completions += 1;
                }
            }
            if (count as usize) < BATCH {
                return parked;
            }
            // A full batch: more may be queued. Take it without waiting.
            timeout = 0;
        }
    }

    fn complete(&mut self, entry: OVERLAPPED_ENTRY) {
        let record = entry.lpOverlapped.cast::<Record>();
        // SAFETY: every OVERLAPPED this port reports is the head of a
        // record made by `start`, alive until this very report.
        let rec = unsafe { &mut *record };
        self.inflight -= 1;
        // `Internal` is the NTSTATUS; 0 is success.
        let mut res = if rec.overlapped.Internal == 0 {
            entry.dwNumberOfBytesTransferred as i32
        } else {
            let mut bytes = 0u32;
            let mut flags = 0u32;
            // SAFETY: the operation has completed, so this only translates
            // the status in the OVERLAPPED; it does not wait.
            let (ok, err) = unsafe {
                if rec.socket {
                    let ok = WSAGetOverlappedResult(
                        rec.handle as SOCKET,
                        &rec.overlapped,
                        &mut bytes,
                        0,
                        &mut flags,
                    );
                    (ok, wsa_error())
                } else {
                    let ok = GetOverlappedResult(rec.handle, &rec.overlapped, &mut bytes, 0);
                    (ok, last_error())
                }
            };
            if ok != 0 {
                bytes as i32
            } else if err == ERROR_HANDLE_EOF as i32 {
                0
            } else {
                -err
            }
        };
        match &rec.state {
            State::Orphan(_) => {
                // Nobody waits. The resources go now that the kernel is
                // done with them. A socket accepted for nobody is among
                // them and closes with them.
                // SAFETY: reported, so the kernel no longer holds it.
                drop(unsafe { Box::from_raw(record) });
            }
            State::Waiting(_) => {
                if res >= 0 {
                    let after = Self::finish(rec.after);
                    if after != 0 {
                        res = after;
                    }
                }
                let State::Waiting(waker) = mem::replace(&mut rec.state, State::Done(res)) else {
                    unreachable!()
                };
                waker.wake();
            }
            State::Done(_) => unreachable!("giong: an operation completed twice"),
        }
    }

    /// Cancel everything in flight and wait until the kernel has reported
    /// all of it. `true` once nothing is in flight; `false` if `limit`
    /// passed first.
    pub fn shutdown(&mut self, limit: Duration) -> bool {
        for &record in &self.ops {
            if record.is_null() {
                continue;
            }
            // SAFETY: a non-null slot is a live record.
            let rec = unsafe { &*record };
            if let State::Waiting(_) = rec.state {
                // SAFETY: as in `cancel_op`.
                unsafe { CancelIoEx(rec.handle, &rec.overlapped) };
            }
        }
        let deadline = std::time::Instant::now() + limit;
        while self.inflight > 0 {
            let left = deadline.saturating_duration_since(std::time::Instant::now());
            if left.is_zero() {
                return false;
            }
            self.park(Park::Wait(Some(left.min(Duration::from_millis(50)))));
        }
        true
    }
}

impl Drop for Driver {
    fn drop(&mut self) {
        // Records the kernel still holds must outlive it: wait for them
        // briefly, and leak whatever is left rather than free it.
        self.shutdown(Duration::from_secs(5));
        for &record in &self.ops {
            if record.is_null() {
                continue;
            }
            // SAFETY: a non-null slot is a live record.
            if let State::Done(_) = unsafe { &(*record).state } {
                // SAFETY: completed and never collected.
                drop(unsafe { Box::from_raw(record) });
            }
        }
    }
}
