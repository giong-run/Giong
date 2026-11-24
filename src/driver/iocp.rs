//! The Windows driver: one I/O completion port per worker.
//!
//! Completion ports are completion-based, as io_uring is: an operation is
//! handed to the kernel together with its buffer, and the port reports it
//! once. So this driver keeps the contract of [`super`] directly. A send or
//! a one-off receive is one overlapped call into the caller's buffer;
//! nothing is copied and nothing is retried.
//!
//! # What stays alive, and until when
//!
//! Each operation in flight has a [`Record`] on the heap. The kernel holds
//! its address (it begins with the `OVERLAPPED`), so a record is freed in
//! exactly two places: when the call that would have started the operation
//! says nothing is in flight, and when the port reports its completion.
//! Dropping a future asks the kernel to cancel (`CancelIoEx`) and moves the
//! operation's resources into the record. Cancellation on Windows is a
//! request: the completion still arrives, aborted or with the result of an
//! operation that won the race, and only then do the resources go.
//!
//! # Results inside the call
//!
//! Handles are set to skip the port when a call succeeds at once
//! (`FILE_SKIP_COMPLETION_PORT_ON_SUCCESS`): a send into a socket with
//! room costs one system call and no turn of the event loop.
//!
//! # Multishot operations
//!
//! Windows has none, so they are "try when polled", as on the readiness
//! drivers, with the waiting done by the port:
//!
//! * A stream receive first reads without blocking into a pool buffer. If
//!   the socket is empty it starts a receive of zero bytes, which holds no
//!   buffer and completes when data (or the end of the stream) is there.
//!   An idle connection therefore pins no pool buffer.
//! * An accept keeps one `AcceptEx` in flight, on a socket created ahead.
//!
//! # Results
//!
//! A result is the number of bytes transferred, or the negated Windows
//! error code, which `io::Error::from_raw_os_error(-res)` reads.

use std::any::Any;
use std::collections::{HashMap, HashSet};
use std::ffi::c_void;
use std::io;
use std::mem;
use std::ptr;
use std::rc::Rc;
use std::sync::Arc;
use std::task::{Poll, Waker};

use windows_sys::Win32::Foundation::{
    CloseHandle, GetLastError, ERROR_BROKEN_PIPE, ERROR_HANDLE_EOF, ERROR_IO_PENDING,
    ERROR_MORE_DATA, ERROR_NETNAME_DELETED, ERROR_NOT_SUPPORTED, ERROR_OPERATION_ABORTED, HANDLE,
    INVALID_HANDLE_VALUE, WAIT_TIMEOUT,
};
use windows_sys::Win32::Networking::WinSock::{
    bind, closesocket, getsockname, getsockopt, ioctlsocket, recv, recvfrom, setsockopt,
    TransmitFile, WSAGetLastError, WSAGetOverlappedResult, WSAIoctl, WSARecv, WSARecvFrom, WSASend,
    WSASendMsg, WSASendTo, WSASocketW, AF_INET6, FIONBIO, INVALID_SOCKET, LPFN_ACCEPTEX,
    LPFN_CONNECTEX, LPFN_WSARECVMSG, MSG_TRUNC, SIO_GET_EXTENSION_FUNCTION_POINTER, SOCKADDR,
    SOCKADDR_IN, SOCKADDR_IN6, SOCKADDR_STORAGE, SOCKET, SOCK_STREAM, SOL_SOCKET, SO_TYPE,
    SO_UPDATE_ACCEPT_CONTEXT, SO_UPDATE_CONNECT_CONTEXT, WSABUF, WSAECONNABORTED, WSAECONNRESET,
    WSAEINVAL, WSAEMSGSIZE, WSAENOTCONN, WSAEOPNOTSUPP, WSAEWOULDBLOCK, WSAID_ACCEPTEX,
    WSAID_CONNECTEX, WSAID_WSARECVMSG, WSA_FLAG_NO_HANDLE_INHERIT, WSA_FLAG_OVERLAPPED,
};
use windows_sys::Win32::Networking::WinSock::{
    SO_PROTOCOL_INFOW, WSAPROTOCOL_INFOW, XP1_IFS_HANDLES,
};
use windows_sys::Win32::Storage::FileSystem::{
    ReadFile, SetFileCompletionNotificationModes, WriteFile,
};
use windows_sys::Win32::System::IO::{
    CancelIoEx, CreateIoCompletionPort, GetOverlappedResult, GetQueuedCompletionStatusEx,
    PostQueuedCompletionStatus, OVERLAPPED, OVERLAPPED_ENTRY,
};

mod alarm;

use super::budget::spend_budget;
use super::{Io, MultiIo, MultiKind, MultiPoll, Park, Start, BUFFER_SHIFT, F_BUFFER, F_MORE};
use crate::buf::{BufPool, RecvBuf};
use crate::slab::Slab;

/// A call that succeeds at once reports nothing to the port.
const FILE_SKIP_COMPLETION_PORT_ON_SUCCESS: u8 = 1;
/// No event is set on the handle either; nobody waits on one.
const FILE_SKIP_SET_EVENT_ON_HANDLE: u8 = 2;

/// Bytes `AcceptEx` needs for each of the two addresses it writes: the
/// largest address plus 16, as its documentation requires.
const ACCEPT_ADDR_LEN: usize = mem::size_of::<SOCKADDR_STORAGE>() + 16;

/// How many completions one call to the kernel collects.
const BATCH: usize = 256;

/// The completion port. Closed when the driver and every wake handle are
/// gone.
pub(crate) struct Port(HANDLE);

// SAFETY: a completion port may be used from any thread; posting to it and
// waiting on it are thread safe by its contract.
unsafe impl Send for Port {}
// SAFETY: as above.
unsafe impl Sync for Port {}

impl Port {
    /// Wakes the worker that waits on the port, or makes its next wait
    /// return at once: a packet with no operation behind it.
    pub(crate) fn post(&self) {
        // SAFETY: the port is open while `self` lives.
        while unsafe { PostQueuedCompletionStatus(self.0, 0, 0, ptr::null()) } == 0 {
            // The kernel could not allocate the packet. A worker that waits
            // without a timer or I/O would never wake: try again.
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
    }

    /// Reports the result of a blocking job as the completion of the
    /// record at `record`. Called on the blocking thread, which never
    /// reads the record: only the worker does, when the packet arrives.
    fn report(&self, record: usize, res: i32) {
        loop {
            // SAFETY: the port is open while `self` lives; the address is
            // only handed back to the worker.
            let ok = unsafe {
                PostQueuedCompletionStatus(self.0, res as u32, 0, record as *const OVERLAPPED)
            };
            if ok != 0 {
                return;
            }
            // The kernel could not allocate the packet. The operation's
            // task waits for it, so try again rather than lose it.
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }
}

/// A job on its way to a blocking thread. Until it reports, only that
/// thread uses what the pointer names (see [`super::BlockingJob`]).
struct SendJob(*mut dyn super::BlockingJob);

// SAFETY: the job is `Send`, and the worker leaves it alone until the
// report.
unsafe impl Send for SendJob {}

impl SendJob {
    fn run(self) -> i32 {
        // SAFETY: `Op` keeps the job alive and unmoved until the report.
        unsafe { (*self.0).run() }
    }
}

impl Drop for Port {
    fn drop(&mut self) {
        // SAFETY: the handle is a port this value owns, closed once.
        unsafe { CloseHandle(self.0) };
    }
}

/// What a socket needs once its `AcceptEx` or `ConnectEx` has succeeded,
/// before it behaves like a connected socket (`shutdown`, `getpeername`).
#[derive(Clone, Copy)]
enum After {
    Nothing,
    Accepted {
        listener: SOCKET,
    },
    Connected {
        socket: SOCKET,
    },
    /// A receive into `room` bytes. Winsock fails one whose datagram is
    /// longer with `WSAEMSGSIZE` and the buffer full, where Unix reports
    /// the bytes that fit: the driver reports those too.
    Received {
        room: i32,
    },
}

/// The socket an `AcceptEx` fills in and the addresses it writes. Owned by
/// the operation's record, so both live until the completion; the socket
/// is closed with it unless the accept handed it on.
struct Accepting {
    socket: SOCKET,
    addrs: [u8; 2 * ACCEPT_ADDR_LEN],
}

impl Drop for Accepting {
    fn drop(&mut self) {
        if self.socket != INVALID_SOCKET {
            // SAFETY: a socket this value created and still owns.
            unsafe { closesocket(self.socket) };
        }
    }
}

enum State {
    /// In flight; its future waits.
    Waiting(Waker),
    /// Completed; the future has not collected the result yet.
    Done(i32),
    /// The future is gone. The resources the kernel may still touch wait
    /// here for the completion.
    Orphan(Option<Box<dyn Any>>),
    /// The waiting part of the multishot operation under this key.
    Multi(usize),
    /// Ended inside the call with a warning status and reported then; a
    /// completion packet may still name it (see [`ended_with_a_warning`]).
    /// The kernel no longer writes to its buffers.
    Lingering,
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
    /// A send on a socket: on a stream, it is not cancelled (see
    /// [`Iocp::cancel_op`]).
    sending: bool,
    /// A read of a file or pipe: a pipe whose writer is gone ends it like
    /// the end of a file.
    reading: bool,
    /// A blocking job's flag, which a job that has not started yet checks.
    cancel: Option<Arc<super::blocking::Cancel>>,
    accepting: Option<Box<Accepting>>,
    datagram: Option<Box<Datagram>>,
}

/// What a datagram receive that waits in the kernel writes to: the buffer,
/// laid out as the readers of a multishot `recvmsg` expect it (see
/// [`MultiIo::RecvMsg`]), and the two values Winsock fills in when the
/// datagram arrives. Boxed, so that they stay where the kernel was told.
struct Datagram {
    buf: Vec<u8>,
    /// Bytes in front of the payload: the header and the address field.
    overhead: usize,
    from_len: i32,
    flags: u32,
}

/// Whether an overlapped call that returned `code` ended at once with a
/// warning status, not an error: a datagram or pipe message longer than
/// the buffer (`STATUS_BUFFER_OVERFLOW`, which Winsock reports as
/// `WSAEMSGSIZE` and files as `ERROR_MORE_DATA`). The data is in the
/// buffer, so the result is reported at once. But the I/O manager may
/// still queue a completion for it: `FILE_SKIP_COMPLETION_PORT_ON_SUCCESS`
/// skips the packet only for success. So the record is not freed: it
/// lingers ([`State::Lingering`]) and goes when its packet comes.
fn ended_with_a_warning(code: i32) -> bool {
    code == WSAEMSGSIZE || code == ERROR_MORE_DATA as i32
}

/// Whether a failed accept (`res`, negated) failed for its connection
/// alone, which its peer reset or abandoned before the accept took it:
/// `ERROR_NETNAME_DELETED` is how a completed `AcceptEx` reports that, and
/// `WSAENOTCONN` comes from readying a socket whose connection is gone.
/// Unix accepts never report these; the listener is fine.
fn connection_gone(res: i32) -> bool {
    [
        ERROR_NETNAME_DELETED as i32,
        WSAECONNRESET,
        WSAECONNABORTED,
        WSAENOTCONN,
    ]
    .contains(&-res)
}

/// Whether `socket` is a stream socket.
fn is_stream(socket: SOCKET) -> bool {
    let mut kind = 0i32;
    let mut len = mem::size_of::<i32>() as i32;
    // SAFETY: an open socket; the value is a local of the size given.
    let rc = unsafe {
        getsockopt(
            socket,
            SOL_SOCKET,
            SO_TYPE,
            (&mut kind as *mut i32).cast(),
            &mut len,
        )
    };
    rc == 0 && kind == SOCK_STREAM
}

/// Whether the provider behind `socket` hands out real file handles
/// (`XP1_IFS_HANDLES`), which the I/O manager treats as files; a layered
/// one that does not may report a call that succeeded at once anyway.
fn ifs_handles(socket: SOCKET) -> bool {
    // SAFETY: plain data, filled in by the call.
    let mut info: WSAPROTOCOL_INFOW = unsafe { mem::zeroed() };
    let mut len = mem::size_of::<WSAPROTOCOL_INFOW>() as i32;
    // SAFETY: an open socket; `info` has room for `len` bytes.
    let rc = unsafe {
        getsockopt(
            socket,
            SOL_SOCKET,
            SO_PROTOCOL_INFOW,
            (&mut info as *mut WSAPROTOCOL_INFOW).cast(),
            &mut len,
        )
    };
    rc == 0 && info.dwServiceFlags1 & XP1_IFS_HANDLES != 0
}

/// Bytes of the header in front of every datagram of a multishot receive:
/// name length, control length, payload length and flags, as `u32`s.
const DATAGRAM_HEADER: usize = 16;

/// Writes that header for a datagram received into the buffer at `buf`:
/// `res` is what the receive returned. A datagram longer than its `room`
/// is cut there, which Winsock reports as `WSAEMSGSIZE` with the buffer
/// full. The length of the datagram in the buffer, or the error.
///
/// # Safety
///
/// `buf` must be valid for [`DATAGRAM_HEADER`] bytes.
unsafe fn finish_datagram(
    buf: *mut u8,
    from_len: i32,
    res: i32,
    room: usize,
) -> Result<usize, i32> {
    let (payload, flags) = match res {
        n if n >= 0 => (n as usize, 0),
        n if n == -WSAEMSGSIZE => (room, MSG_TRUNC),
        e => return Err(e),
    };
    let fields = [from_len.max(0) as u32, 0, payload as u32, flags];
    for (i, field) in fields.iter().enumerate() {
        ptr::copy_nonoverlapping(field.to_ne_bytes().as_ptr(), buf.add(i * 4), 4);
    }
    Ok(payload)
}

/// A multishot operation: what it runs on, who waits for it, and the one
/// kernel operation it may have in flight.
struct Multi {
    io: Option<MultiIo>,
    /// The tasks that wait for it: several may receive on one socket, and
    /// each of them is woken when something arrives.
    wakers: Vec<Waker>,
    /// The zero-byte receive or `AcceptEx` in flight; null if none.
    pending: *mut Record,
    /// An accept that completed and was not collected yet, or the error
    /// that ended a receive.
    accepted: Option<i32>,
    /// A datagram that arrived while its receive waited in the kernel.
    data: Option<RecvBuf>,
    /// Set by [`Iocp::finish_multi`]: nothing new is started.
    last: bool,
}

impl Multi {
    /// Has `waker` woken when the operation in flight completes.
    fn wait(&mut self, waker: &Waker) {
        if !self.wakers.iter().any(|w| w.will_wake(waker)) {
            self.wakers.push(waker.clone());
        }
    }
}

enum Slot {
    Single(*mut Record),
    Multi(Multi),
}

pub(crate) struct Iocp {
    port: Arc<Port>,
    ops: Slab<Slot>,
    /// Records the kernel holds, orphans included.
    inflight: usize,
    events: Vec<OVERLAPPED_ENTRY>,
    /// Completions collected by `has_completions` and not processed yet.
    ready: usize,
    /// Handles associated with the port, each with whether the port skips
    /// the completion of a call that succeeds at once (see `register`).
    registered: HashMap<usize, bool>,
    pool: Rc<BufPool>,
    graveyard: Vec<Box<dyn Any>>,
    /// Records whose future is gone and that the kernel or a blocking
    /// thread still holds; `shutdown` cancels them.
    orphans: HashSet<usize>,
    /// Records of calls that ended at once with a warning, kept until the
    /// packet that may still name them comes (see `State::Lingering`).
    lingering: HashSet<usize>,
    accept_ex: LPFN_ACCEPTEX,
    connect_ex: LPFN_CONNECTEX,
    recv_msg: LPFN_WSARECVMSG,
    /// Ends a wait at the next timer to the microsecond, where the wait's
    /// own timeout ends on a 15.6 ms clock tick (see [`alarm`]).
    alarm: Option<alarm::Alarm>,
}

fn last_error() -> i32 {
    // SAFETY: reads the calling thread's last error code.
    unsafe { GetLastError() as i32 }
}

fn wsa_error() -> i32 {
    // SAFETY: reads the calling thread's last socket error code.
    unsafe { WSAGetLastError() }
}

/// The result of a call that returns 0 on success and whose failure code
/// is the thread's last socket error.
fn wsa(rc: i32) -> Result<(), i32> {
    if rc == 0 {
        Ok(())
    } else {
        Err(wsa_error())
    }
}

/// Completion flags for data in pool buffer `bid`.
#[allow(dead_code)] // The datagram receive reports through flags.
fn pooled_flags(bid: u16) -> u32 {
    F_BUFFER | (u32::from(bid) << BUFFER_SHIFT) | F_MORE
}

impl Iocp {
    pub(crate) fn new(pool: Rc<BufPool>) -> io::Result<Self> {
        // SAFETY: creates a new port; no handle is associated yet. One
        // thread waits on it, hence a concurrency of 1.
        let port = unsafe { CreateIoCompletionPort(INVALID_HANDLE_VALUE, ptr::null_mut(), 0, 1) };
        if port.is_null() {
            return Err(io::Error::last_os_error());
        }
        Ok(Self {
            port: Arc::new(Port(port)),
            ops: Slab::new(),
            inflight: 0,
            // SAFETY: OVERLAPPED_ENTRY is plain data; all-zero is valid.
            events: vec![unsafe { mem::zeroed() }; BATCH],
            ready: 0,
            registered: HashMap::new(),
            pool,
            graveyard: Vec::new(),
            orphans: HashSet::new(),
            lingering: HashSet::new(),
            accept_ex: None,
            connect_ex: None,
            recv_msg: None,
            alarm: alarm::Alarm::new(),
        })
    }

    pub(crate) fn waker(&self) -> super::WakeHandle {
        super::WakeHandle::Port(self.port.clone())
    }

    pub(crate) fn pool(&self) -> &Rc<BufPool> {
        &self.pool
    }

    // ---------------------------------------------------------------- handles

    /// Associates a handle with the port, once. A socket is also put in
    /// non-blocking mode, for the reads the multishot receive tries first;
    /// overlapped calls are not affected by it.
    ///
    /// A call that succeeds at once is not reported through the port too
    /// (`FILE_SKIP_COMPLETION_PORT_ON_SUCCESS`), except on a socket whose
    /// provider does not hand out real file handles: a layered provider of
    /// some antivirus or VPN software can still report it, so there every
    /// call counts as in flight until its completion, as libuv does.
    fn register(&mut self, handle: HANDLE, socket: bool) -> Result<(), i32> {
        if self.registered.contains_key(&(handle as usize)) {
            return Ok(());
        }
        // First what can fail without leaving a trace: once the handle has
        // joined the port it cannot join again, so it must be recorded then.
        if socket {
            let mut on = 1u32;
            // SAFETY: a socket the caller owns; the argument is a local.
            if unsafe { ioctlsocket(handle as SOCKET, FIONBIO, &mut on) } != 0 {
                return Err(-wsa_error());
            }
        }
        // SAFETY: associates a handle the caller owns with our open port;
        // a handle that cannot be associated makes the call fail.
        let port = unsafe { CreateIoCompletionPort(handle, self.port.0, 0, 0) };
        if port.is_null() {
            return Err(-last_error());
        }
        let skips = !socket || ifs_handles(handle as SOCKET);
        let modes = if skips {
            FILE_SKIP_SET_EVENT_ON_HANDLE | FILE_SKIP_COMPLETION_PORT_ON_SUCCESS
        } else {
            FILE_SKIP_SET_EVENT_ON_HANDLE
        };
        // SAFETY: sets notification modes on a handle the caller owns. Where
        // that fails (a layered provider may refuse it), every call waits
        // for its packet, which is always safe.
        let skips = skips && unsafe { SetFileCompletionNotificationModes(handle, modes) } != 0;
        self.registered.insert(handle as usize, skips);
        Ok(())
    }

    /// Whether `result`, what an overlapped call on `handle` returned, means
    /// that its completion will come through the port: in flight, or
    /// finished at once on a handle whose port does not skip that.
    fn reported_later(&self, handle: HANDLE, result: &Result<(), i32>) -> bool {
        match result {
            Err(code) => *code == ERROR_IO_PENDING as i32,
            Ok(()) => !self
                .registered
                .get(&(handle as usize))
                .copied()
                .unwrap_or(true),
        }
    }

    /// Closes a file handle the caller gives up. Operations in flight on
    /// it complete, aborted.
    pub(crate) fn close_fd(&mut self, handle: HANDLE) {
        self.registered.remove(&(handle as usize));
        // SAFETY: the caller owns the handle and gives it up here.
        unsafe { CloseHandle(handle) };
    }

    /// Closes a socket the caller gives up.
    pub(crate) fn close_sock(&mut self, socket: SOCKET) {
        self.registered.remove(&socket);
        // SAFETY: the caller owns the socket and gives it up here.
        unsafe { closesocket(socket) };
    }

    /// A function a socket's provider hands out (`AcceptEx`, `ConnectEx`,
    /// `WSARecvMsg`). `F` is `Option` of the function's type.
    fn extension<F: Copy>(socket: SOCKET, id: windows_sys::core::GUID) -> Result<F, i32> {
        debug_assert_eq!(mem::size_of::<F>(), mem::size_of::<usize>());
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
        wsa(rc)?;
        // SAFETY: `F` is `Option<extern fn>`, which has the layout of a
        // pointer with null as `None`; the provider returned a function of
        // this type for this identifier.
        Ok(unsafe { mem::transmute_copy::<usize, F>(&function) })
    }

    // ----------------------------------------------------------- single-shot

    fn record(handle: HANDLE, socket: bool, state: State, offset: u64) -> *mut Record {
        // SAFETY: OVERLAPPED is plain data; all-zero is its initial state.
        let mut overlapped: OVERLAPPED = unsafe { mem::zeroed() };
        overlapped.Anonymous.Anonymous.Offset = offset as u32;
        overlapped.Anonymous.Anonymous.OffsetHigh = (offset >> 32) as u32;
        Box::into_raw(Box::new(Record {
            overlapped,
            handle,
            socket,
            after: After::Nothing,
            state,
            sending: false,
            reading: false,
            cancel: None,
            accepting: None,
            datagram: None,
        }))
    }

    /// Begins a single-shot operation.
    pub(crate) fn start(&mut self, io: &Io, waker: &Waker) -> Start {
        let (handle, socket, offset) = match *io {
            Io::Connect { fd, .. }
            | Io::Recv { fd, .. }
            | Io::Send { fd, .. }
            | Io::SendMsg { fd, .. }
            | Io::SendTo { fd, .. }
            | Io::RecvMsg { fd, .. } => (fd as HANDLE, true, 0),
            Io::SendFile { fd, offset, .. } => (fd as HANDLE, true, offset),
            Io::ReadReady { fd, .. } => (fd, false, 0),
            Io::Read { fd, offset, .. } | Io::Write { fd, offset, .. } => (fd, false, offset),
            // Windows reports no "writable": a send waits by itself.
            Io::Writable { .. } => return Start::Done(0, 0),
            Io::Close { fd } => {
                self.registered.remove(&(fd as usize));
                // SAFETY: the caller owns the handle and gives it up here.
                let ok = unsafe { CloseHandle(fd) };
                return Start::Done(if ok != 0 { 0 } else { -last_error() }, 0);
            }
            Io::Blocking { job } => {
                let record = Self::record(ptr::null_mut(), false, State::Waiting(waker.clone()), 0);
                let cancel = super::blocking::Cancel::new();
                // SAFETY: the record was just made and is ours.
                unsafe { (*record).cancel = Some(cancel.clone()) };
                self.inflight += 1;
                let (port, job, at) = (self.port.clone(), SendJob(job), record as usize);
                super::blocking::spawn(Box::new(move || {
                    // A job whose future went before it started does not
                    // run: a removal or a rename could otherwise act much
                    // later, on a file made since.
                    let res = if cancel.is_cancelled() {
                        -(ERROR_OPERATION_ABORTED as i32)
                    } else {
                        job.run()
                    };
                    port.report(at, res);
                }));
                return Start::Submitted(self.ops.insert(Slot::Single(record)));
            }
            // The file API reaches these through `Io::Blocking`.
            Io::OpenAt { .. }
            | Io::Fsync { .. }
            | Io::Fallocate { .. }
            | Io::UnlinkAt { .. }
            | Io::MkDirAt { .. }
            | Io::RenameAt { .. } => return Start::Done(-(ERROR_NOT_SUPPORTED as i32), 0),
        };
        if let Err(e) = self.register(handle, socket) {
            return Start::Done(e, 0);
        }
        let record = Self::record(handle, socket, State::Waiting(waker.clone()), offset);
        let ov = record.cast::<OVERLAPPED>();
        let mut transferred = 0u32;
        // SAFETY: the record was just made; `Op` guarantees the header and
        // the buffers of a `RecvMsg`.
        unsafe {
            match *io {
                Io::Recv { len, .. } => {
                    (*record).after = After::Received {
                        room: len.min(i32::MAX as u32) as i32,
                    }
                }
                Io::Send { .. } | Io::SendMsg { .. } | Io::SendFile { .. } => {
                    (*record).sending = true;
                }
                Io::Read { .. } | Io::ReadReady { .. } => (*record).reading = true,
                Io::RecvMsg { msg, .. } => {
                    let bufs =
                        std::slice::from_raw_parts((*msg).lpBuffers, (*msg).dwBufferCount as usize);
                    let room: u64 = bufs.iter().map(|b| u64::from(b.len)).sum();
                    (*record).after = After::Received {
                        room: room.min(i32::MAX as u64) as i32,
                    };
                }
                _ => {}
            }
        }

        // Each arm leaves `Ok(())` for "the call succeeded at once" and
        // `Err(code)` otherwise, where ERROR_IO_PENDING means in flight.
        //
        // SAFETY: in every arm, `Op` guarantees the handle and the
        // pointers of `io`; `ov` points at the record, which lives until
        // its completion.
        let outcome: Result<(), i32> = unsafe {
            match *io {
                Io::Connect { fd, addr, len } => {
                    (*record).after = After::Connected { socket: fd };
                    self.connect(fd, addr, len, ov)
                }
                Io::Recv { fd, buf, len } => {
                    let wsabuf = WSABUF { len, buf };
                    let mut flags = 0u32;
                    wsa(WSARecv(
                        fd,
                        &wsabuf,
                        1,
                        &mut transferred,
                        &mut flags,
                        ov,
                        None,
                    ))
                }
                Io::Send { fd, buf, len, .. } => {
                    let wsabuf = WSABUF {
                        len,
                        buf: buf.cast_mut(),
                    };
                    wsa(WSASend(fd, &wsabuf, 1, &mut transferred, 0, ov, None))
                }
                Io::SendMsg { fd, msg, .. } => {
                    let m = &*msg;
                    if m.Control.len != 0 {
                        // Ancillary data: only `WSASendMsg` carries it,
                        // and only datagram sockets take that call.
                        wsa(WSASendMsg(fd, msg, 0, &mut transferred, ov, None))
                    } else if m.name.is_null() {
                        // What a vectored write on a stream is: Winsock
                        // refuses `WSASendMsg` on a stream socket.
                        wsa(WSASend(
                            fd,
                            m.lpBuffers,
                            m.dwBufferCount,
                            &mut transferred,
                            0,
                            ov,
                            None,
                        ))
                    } else {
                        wsa(WSASendTo(
                            fd,
                            m.lpBuffers,
                            m.dwBufferCount,
                            &mut transferred,
                            0,
                            m.name,
                            m.namelen,
                            ov,
                            None,
                        ))
                    }
                }
                Io::SendTo {
                    fd,
                    buf,
                    len,
                    addr,
                    addr_len,
                } => {
                    let wsabuf = WSABUF {
                        len,
                        buf: buf.cast_mut(),
                    };
                    wsa(WSASendTo(
                        fd,
                        &wsabuf,
                        1,
                        &mut transferred,
                        0,
                        addr,
                        addr_len,
                        ov,
                        None,
                    ))
                }
                Io::RecvMsg { fd, msg } => match self.recv_msg_for(fd) {
                    Err(code) => Err(code),
                    Ok(recv_msg) => wsa(recv_msg(fd, msg, &mut transferred, ov, None)),
                },
                Io::ReadReady { fd, buf, len } | Io::Read { fd, buf, len, .. } => {
                    if ReadFile(fd, buf, len, &mut transferred, ov) != 0 {
                        Ok(())
                    } else {
                        Err(last_error())
                    }
                }
                Io::Write { fd, buf, len, .. } => {
                    if WriteFile(fd, buf, len, &mut transferred, ov) != 0 {
                        Ok(())
                    } else {
                        Err(last_error())
                    }
                }
                Io::SendFile { fd, file, len, .. } => {
                    if TransmitFile(fd, file, len, 0, ov, ptr::null(), 0) != 0 {
                        // The call has no place for the count: it is in
                        // the OVERLAPPED.
                        transferred = (*ov).InternalHigh as u32;
                        Ok(())
                    } else {
                        Err(wsa_error())
                    }
                }
                _ => unreachable!("handled above"),
            }
        };
        match outcome {
            ref r if self.reported_later(handle, r) => {
                self.inflight += 1;
                Start::Submitted(self.ops.insert(Slot::Single(record)))
            }
            Err(code) if ended_with_a_warning(code) => {
                // SAFETY: the call has ended, so the kernel no longer
                // writes to the record; its packet may still name it.
                let (after, count) = unsafe {
                    (*record).state = State::Lingering;
                    ((*record).after, (*record).overlapped.InternalHigh)
                };
                self.lingering.insert(record as usize);
                let res = if socket {
                    Self::cut(after, -code)
                } else {
                    // Part of a pipe message: what came, as a stream read.
                    count.min(i32::MAX as usize) as i32
                };
                Start::Done(res, 0)
            }
            done => {
                // Nothing is in flight: no completion will name the record.
                // SAFETY: it came from `Box::into_raw` and the kernel does
                // not hold it.
                let record = unsafe { Box::from_raw(record) };
                let res = match done {
                    Ok(()) => match Self::finish(record.after, INVALID_SOCKET) {
                        0 => transferred as i32,
                        err => err,
                    },
                    Err(code) if Self::is_end(&record, code) => 0,
                    Err(code) => Self::cut(record.after, -code),
                };
                Start::Done(res, 0)
            }
        }
    }

    /// `ConnectEx`, which wants a bound socket: one that is not bound yet
    /// is bound to any address of the peer's family first.
    ///
    /// # Safety
    ///
    /// `addr` is valid for `len` bytes and `ov` heads a live record.
    unsafe fn connect(
        &mut self,
        socket: SOCKET,
        addr: *const SOCKADDR,
        len: i32,
        ov: *mut OVERLAPPED,
    ) -> Result<(), i32> {
        if self.connect_ex.is_none() {
            self.connect_ex = Self::extension(socket, WSAID_CONNECTEX)?;
        }
        let connect_ex = self.connect_ex.ok_or(WSAEOPNOTSUPP)?;
        // SAFETY: the caller guarantees `addr`; the wildcard addresses are
        // zeroed locals of their family's size.
        unsafe {
            let rc = if (*addr).sa_family == AF_INET6 {
                let mut any: SOCKADDR_IN6 = mem::zeroed();
                any.sin6_family = AF_INET6;
                bind(
                    socket,
                    (&any as *const SOCKADDR_IN6).cast(),
                    mem::size_of::<SOCKADDR_IN6>() as i32,
                )
            } else {
                let mut any: SOCKADDR_IN = mem::zeroed();
                any.sin_family = (*addr).sa_family;
                bind(
                    socket,
                    (&any as *const SOCKADDR_IN).cast(),
                    mem::size_of::<SOCKADDR_IN>() as i32,
                )
            };
            // WSAEINVAL: bound already, by the caller.
            if rc != 0 && wsa_error() != WSAEINVAL {
                return Err(wsa_error());
            }
            if connect_ex(socket, addr, len, ptr::null(), 0, ptr::null_mut(), ov) != 0 {
                Ok(())
            } else {
                Err(wsa_error())
            }
        }
    }

    fn recv_msg_for(
        &mut self,
        socket: SOCKET,
    ) -> Result<
        unsafe extern "system" fn(
            SOCKET,
            *mut windows_sys::Win32::Networking::WinSock::WSAMSG,
            *mut u32,
            *mut OVERLAPPED,
            windows_sys::Win32::Networking::WinSock::LPWSAOVERLAPPED_COMPLETION_ROUTINE,
        ) -> i32,
        i32,
    > {
        if self.recv_msg.is_none() {
            self.recv_msg = Self::extension(socket, WSAID_WSARECVMSG)?;
        }
        self.recv_msg.ok_or(WSAEOPNOTSUPP)
    }

    /// Whether a failed read (`code`) is the end of its file or pipe.
    fn is_end(rec: &Record, code: i32) -> bool {
        code == ERROR_HANDLE_EOF as i32 || (rec.reading && code == ERROR_BROKEN_PIPE as i32)
    }

    /// The result `res` of an operation, with a datagram that was cut to
    /// fit its buffer reported as the bytes that fit.
    fn cut(after: After, res: i32) -> i32 {
        match after {
            After::Received { room } if res == -WSAEMSGSIZE => room,
            _ => res,
        }
    }

    /// Runs what a successful accept or connect still needs. 0, or the
    /// negated error.
    fn finish(after: After, accepted: SOCKET) -> i32 {
        let rc = match after {
            After::Nothing | After::Received { .. } => 0,
            // SAFETY: both sockets are open (the operation just succeeded
            // on them); the option value is the listener's handle.
            After::Accepted { listener } => unsafe {
                setsockopt(
                    accepted,
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

    /// The result of a single-shot operation, once the port has reported
    /// it.
    pub(crate) fn poll_op(&mut self, key: usize, waker: &Waker) -> Poll<(i32, u32)> {
        let Some(&Slot::Single(record)) = self.ops.get(key) else {
            panic!("giong: polling an invalid operation");
        };
        // SAFETY: a key names a live record until `poll_op` returns Ready
        // or `cancel_op` is called for it.
        match unsafe { &mut (*record).state } {
            State::Done(res) => {
                let res = *res;
                // SAFETY: completed, so the kernel no longer holds it.
                drop(unsafe { Box::from_raw(record) });
                self.ops.remove(key);
                Poll::Ready((res, 0))
            }
            State::Waiting(current) => {
                if !current.will_wake(waker) {
                    *current = waker.clone();
                }
                Poll::Pending
            }
            State::Orphan(_) | State::Multi(_) | State::Lingering => {
                unreachable!("giong: an operation was polled after its drop")
            }
        }
    }

    /// The future of an operation was dropped before it collected the
    /// result. The driver keeps `data` until the kernel has reported the
    /// operation. With `finish` the operation is left to run to its end.
    pub(crate) fn cancel_op(
        &mut self,
        key: usize,
        data: Option<Box<dyn Any>>,
        _result_is_fd: bool,
        finish: bool,
    ) {
        let Some(Slot::Single(record)) = self.ops.remove(key) else {
            panic!("giong: cancelling an invalid operation");
        };
        // SAFETY: as in `poll_op`.
        let rec = unsafe { &mut *record };
        if let State::Done(_) = rec.state {
            // Already reported: nothing is in flight.
            // SAFETY: as in `poll_op`.
            drop(unsafe { Box::from_raw(record) });
            self.graveyard.extend(data);
            return;
        }
        rec.state = State::Orphan(data);
        self.orphans.insert(record as usize);
        // Windows aborts a connection whose send is cancelled part way, so
        // the stream would be unusable after a dropped write, where Unix
        // keeps it: a send on a stream runs to its end instead.
        if !finish && !(rec.sending && is_stream(rec.handle as SOCKET)) {
            Self::cancel(rec);
        }
    }

    /// Asks the kernel to cancel one operation. Its completion arrives
    /// either way: aborted, or with the result if it had finished already.
    fn cancel(rec: &Record) {
        if let Some(cancel) = &rec.cancel {
            // A blocking job: it reports either way.
            cancel.cancel();
            return;
        }
        // SAFETY: names one operation by its OVERLAPPED, on the handle it
        // was started on. If the handle is closed the call fails and the
        // completion still comes.
        unsafe { CancelIoEx(rec.handle, &rec.overlapped) };
    }

    // ------------------------------------------------------------- multishot

    pub(crate) fn new_multi(&mut self, _kind: MultiKind) -> usize {
        self.ops.insert(Slot::Multi(Multi {
            io: None,
            wakers: Vec::new(),
            pending: ptr::null_mut(),
            accepted: None,
            data: None,
            last: false,
        }))
    }

    pub(crate) fn arm_multi(&mut self, key: usize, new: MultiIo) {
        match self.ops.get_mut(key) {
            Some(Slot::Multi(Multi { io: io @ None, .. })) => *io = Some(new),
            _ => panic!("giong: arming an invalid multishot operation"),
        }
    }

    fn multi(&mut self, key: usize) -> &mut Multi {
        match self.ops.get_mut(key) {
            Some(Slot::Multi(multi)) => multi,
            _ => panic!("giong: polling an invalid multishot operation"),
        }
    }

    /// Ends the multishot operation as an error ends one on io_uring: the
    /// next poll asks for it to be armed again.
    fn disarm(&mut self, key: usize) {
        self.multi(key).io = None;
    }

    /// Lets the multishot operation run out: what it has in the kernel
    /// still completes and is reported, and nothing new is started. For a
    /// listener whose connections another thread accepts from now on: the
    /// accept already in flight takes one more connection, which must not
    /// be lost.
    pub(crate) fn finish_multi(&mut self, key: usize) {
        self.multi(key).last = true;
    }

    /// The socket of an accept that completed and was not collected yet,
    /// for a listener that goes away while others share its address; no
    /// further accept is started.
    pub(crate) fn take_accepted(&mut self, key: usize) -> Option<i32> {
        let multi = self.multi(key);
        multi.last = true;
        match multi.accepted {
            Some(socket) if socket >= 0 => multi.accepted.take(),
            _ => None,
        }
    }

    pub(crate) fn poll_multi(&mut self, key: usize, waker: &Waker) -> MultiPoll {
        let multi = self.multi(key);
        if let Some(res) = multi.accepted.take() {
            let accepting = matches!(multi.io, Some(MultiIo::Accept { .. }));
            if !(accepting && connection_gone(res)) {
                if res < 0 {
                    multi.io = None;
                }
                return MultiPoll::Item(res);
            }
            // A connection that its peer reset before the accept took it:
            // the next `AcceptEx` takes the next one, as Unix does.
        }
        if multi.last {
            // What the receive in flight took is still the reader's.
            if let Some(data) = multi.data.take() {
                return MultiPoll::Data(data);
            }
            if !multi.pending.is_null() {
                multi.wait(waker);
            }
            return MultiPoll::Pending;
        }
        let Some(io) = multi.io else {
            return MultiPoll::NeedArm;
        };
        if let Some(data) = multi.data.take() {
            return MultiPoll::Data(data);
        }
        if !multi.pending.is_null() {
            multi.wait(waker);
            return MultiPoll::Pending;
        }
        if !spend_budget() {
            waker.wake_by_ref();
            return MultiPoll::Pending;
        }
        match io {
            MultiIo::Accept { fd } => self.poll_accept(key, fd, waker),
            MultiIo::Recv { fd, .. } => self.poll_recv(key, fd, waker),
            MultiIo::RecvMsg { fd, hdr } => {
                // SAFETY: the header of a multishot receive outlives it.
                let name_len = unsafe { (*hdr).namelen }.max(0) as usize;
                self.poll_recv_msg(key, fd, name_len, waker)
            }
        }
    }

    fn poll_recv(&mut self, key: usize, socket: SOCKET, waker: &Waker) -> MultiPoll {
        if let Err(e) = self.register(socket as HANDLE, true) {
            self.disarm(key);
            return MultiPoll::Item(e);
        }
        // At most twice: a zero-byte receive that completes inside its
        // call says there is data now.
        for _ in 0..2 {
            let Some((bid, buf, len)) = self.pool.next_free() else {
                // No pool buffer: the caller waits for one, then arms again.
                self.disarm(key);
                return MultiPoll::NeedArm;
            };
            // SAFETY: `buf` is a free pool buffer of `len` bytes; the
            // socket is non-blocking, so the call returns at once.
            let n = unsafe { recv(socket, buf, len.min(i32::MAX as usize) as i32, 0) };
            if n > 0 {
                self.pool.take_next();
                return MultiPoll::Data(RecvBuf::pooled(self.pool.clone(), bid, 0, n as u32));
            }
            if n == 0 {
                // End of stream: no buffer. Every later poll reports it
                // again.
                return MultiPoll::Item(0);
            }
            let err = wsa_error();
            if err != WSAEWOULDBLOCK {
                self.disarm(key);
                return MultiPoll::Item(-err);
            }
            // Empty: wait with a receive of zero bytes, which holds no
            // buffer.
            let record = Self::record(socket as HANDLE, true, State::Multi(key), 0);
            let wsabuf = WSABUF {
                len: 0,
                buf: ptr::null_mut(),
            };
            let (mut transferred, mut flags) = (0u32, 0u32);
            // SAFETY: a length of 0 touches no memory; the record lives
            // until its completion.
            let rc = unsafe {
                WSARecv(
                    socket,
                    &wsabuf,
                    1,
                    &mut transferred,
                    &mut flags,
                    record.cast(),
                    None,
                )
            };
            match wsa(rc) {
                ref r if self.reported_later(socket as HANDLE, r) => {
                    self.inflight += 1;
                    let multi = self.multi(key);
                    multi.pending = record;
                    multi.wait(waker);
                    return MultiPoll::Pending;
                }
                done => {
                    // SAFETY: nothing is in flight for this record.
                    drop(unsafe { Box::from_raw(record) });
                    if let Err(code) = done {
                        self.disarm(key);
                        return MultiPoll::Item(-code);
                    }
                    // Data arrived in between: read it.
                }
            }
        }
        // Readable twice and empty twice: another reader took the data.
        waker.wake_by_ref();
        MultiPoll::Pending
    }

    /// The next datagram of `socket`, in front of it the header and the
    /// sender's address in a field of `name_len` bytes.
    ///
    /// A datagram that is already there is read into a pool buffer at once.
    /// Otherwise the receive waits in the kernel. A receive of zero bytes
    /// cannot do the waiting, as it does for a stream: it would take the
    /// datagram and throw it away. So the waiting receive has a buffer, and
    /// that buffer is on the heap: a socket that waits must not hold a pool
    /// buffer, or enough idle sockets would hold them all.
    fn poll_recv_msg(
        &mut self,
        key: usize,
        socket: SOCKET,
        name_len: usize,
        waker: &Waker,
    ) -> MultiPoll {
        if let Err(e) = self.register(socket as HANDLE, true) {
            self.disarm(key);
            return MultiPoll::Item(e);
        }
        let overhead = DATAGRAM_HEADER + name_len;
        let Some((bid, buf, len)) = self.pool.next_free() else {
            // No pool buffer: the caller waits for one, then arms again.
            self.disarm(key);
            return MultiPoll::NeedArm;
        };
        if len <= overhead {
            // Receive buffers too small to hold even the address.
            self.disarm(key);
            return MultiPoll::Item(-WSAEINVAL);
        }
        let room = len - overhead;
        let mut from_len = name_len as i32;
        // SAFETY: `buf` is a free pool buffer of `len` bytes: the payload
        // goes behind the header and the address field, the address into
        // its field. The socket is non-blocking.
        let n = unsafe {
            recvfrom(
                socket,
                buf.add(overhead),
                room.min(i32::MAX as usize) as i32,
                0,
                buf.add(DATAGRAM_HEADER).cast(),
                &mut from_len,
            )
        };
        let res = if n >= 0 { n } else { -wsa_error() };
        if res != -WSAEWOULDBLOCK {
            // SAFETY: the pool buffer is longer than the header.
            return match unsafe { finish_datagram(buf, from_len, res, room) } {
                Ok(payload) => {
                    self.pool.take_next();
                    let len = (overhead + payload) as u32;
                    MultiPoll::Data(RecvBuf::pooled(self.pool.clone(), bid, 0, len))
                }
                Err(e) => {
                    self.disarm(key);
                    MultiPoll::Item(e)
                }
            };
        }
        // Nothing there: wait for the next datagram in the kernel.
        let record = Self::record(socket as HANDLE, true, State::Multi(key), 0);
        let mut datagram = Box::new(Datagram {
            buf: vec![0u8; len],
            overhead,
            from_len: name_len as i32,
            flags: 0,
        });
        let wsabuf = WSABUF {
            len: room.min(u32::MAX as usize) as u32,
            // SAFETY: `overhead < len`, within the buffer.
            buf: unsafe { datagram.buf.as_mut_ptr().add(overhead) },
        };
        // SAFETY: the buffer, the address field and the two values the
        // kernel writes are in the boxed `Datagram`, which the record owns
        // until its completion; the record lives until then too.
        let rc = unsafe {
            let from = datagram.buf.as_mut_ptr().add(DATAGRAM_HEADER).cast();
            let from_len = ptr::addr_of_mut!(datagram.from_len);
            let flags = ptr::addr_of_mut!(datagram.flags);
            (*record).datagram = Some(datagram);
            WSARecvFrom(
                socket,
                &wsabuf,
                1,
                ptr::null_mut(),
                flags,
                from,
                from_len,
                record.cast(),
                None,
            )
        };
        let done = match wsa(rc) {
            ref r if self.reported_later(socket as HANDLE, r) => {
                self.inflight += 1;
                let multi = self.multi(key);
                multi.pending = record;
                multi.wait(waker);
                return MultiPoll::Pending;
            }
            // A datagram arrived in between. Finished within the call, so
            // the port reports nothing.
            Ok(()) => {
                // SAFETY: the operation completed; this reads its count.
                unsafe { (*record).overlapped.InternalHigh as i32 }
            }
            Err(code) if ended_with_a_warning(code) => {
                // Cut off, inside the call: the datagram is in the buffer,
                // but a packet may still name the record, which lingers.
                // SAFETY: the call has ended; the kernel no longer writes
                // to the record or the datagram it owned.
                let mut datagram = unsafe {
                    (*record).state = State::Lingering;
                    (*record).datagram.take().expect("a datagram receive")
                };
                self.lingering.insert(record as usize);
                return match Self::datagram(&mut datagram, -code) {
                    Ok(data) => MultiPoll::Data(data),
                    Err(e) => {
                        self.disarm(key);
                        MultiPoll::Item(e)
                    }
                };
            }
            Err(code) => -code,
        };
        // SAFETY: nothing is in flight for this record.
        let mut record = unsafe { Box::from_raw(record) };
        let mut datagram = record.datagram.take().expect("a datagram receive");
        match Self::datagram(&mut datagram, done) {
            Ok(data) => MultiPoll::Data(data),
            Err(e) => {
                self.disarm(key);
                MultiPoll::Item(e)
            }
        }
    }

    /// The datagram a waiting receive completed with (`res`), as its
    /// reader takes it.
    fn datagram(datagram: &mut Datagram, res: i32) -> Result<RecvBuf, i32> {
        let room = datagram.buf.len() - datagram.overhead;
        // SAFETY: the buffer is longer than the header.
        let payload =
            unsafe { finish_datagram(datagram.buf.as_mut_ptr(), datagram.from_len, res, room) }?;
        let mut buf = mem::take(&mut datagram.buf);
        buf.truncate(datagram.overhead + payload);
        Ok(RecvBuf::heap(buf))
    }

    fn poll_accept(&mut self, key: usize, listener: SOCKET, waker: &Waker) -> MultiPoll {
        if let Err(e) = self.register(listener as HANDLE, true) {
            self.disarm(key);
            return MultiPoll::Item(e);
        }
        loop {
            match self.start_accept(key, listener) {
                // A connection that its peer reset before the accept took
                // it: take the next one, as Unix does. Each try takes one
                // off the backlog, so this ends.
                Ok(Some(res)) | Err(res) if connection_gone(res) => continue,
                Ok(Some(res)) => return MultiPoll::Item(res),
                Ok(None) => {
                    self.multi(key).wait(waker);
                    return MultiPoll::Pending;
                }
                Err(e) => {
                    self.disarm(key);
                    return MultiPoll::Item(e);
                }
            }
        }
    }

    /// Starts one `AcceptEx`. `Some(socket)` if it finished inside the
    /// call, `None` if it is in flight, or the negated error.
    fn start_accept(&mut self, key: usize, listener: SOCKET) -> Result<Option<i32>, i32> {
        if self.accept_ex.is_none() {
            self.accept_ex = Self::extension(listener, WSAID_ACCEPTEX).map_err(|e| -e)?;
        }
        let accept_ex = self.accept_ex.ok_or(-WSAEOPNOTSUPP)?;
        // The socket to accept into has the listener's address family.
        // SAFETY: plain data, filled in by the call below.
        let mut local: SOCKADDR_STORAGE = unsafe { mem::zeroed() };
        let mut len = mem::size_of::<SOCKADDR_STORAGE>() as i32;
        // SAFETY: `local` has room for `len` bytes.
        wsa(unsafe {
            getsockname(
                listener,
                (&mut local as *mut SOCKADDR_STORAGE).cast(),
                &mut len,
            )
        })
        .map_err(|e| -e)?;
        // SAFETY: creates a socket; the result is checked.
        let socket = unsafe {
            WSASocketW(
                i32::from(local.ss_family),
                SOCK_STREAM,
                0,
                ptr::null(),
                0,
                WSA_FLAG_OVERLAPPED | WSA_FLAG_NO_HANDLE_INHERIT,
            )
        };
        if socket == INVALID_SOCKET {
            return Err(-wsa_error());
        }
        let record = Self::record(listener as HANDLE, true, State::Multi(key), 0);
        let mut transferred = 0u32;
        // SAFETY: the record is ours until the call below; the addresses
        // and the socket live in it until the completion.
        let ok = unsafe {
            (*record).after = After::Accepted { listener };
            (*record).accepting = Some(Box::new(Accepting {
                socket,
                addrs: [0; 2 * ACCEPT_ADDR_LEN],
            }));
            let addrs = (*record).accepting.as_mut().unwrap().addrs.as_mut_ptr();
            accept_ex(
                listener,
                socket,
                addrs.cast::<c_void>(),
                0,
                ACCEPT_ADDR_LEN as u32,
                ACCEPT_ADDR_LEN as u32,
                &mut transferred,
                record.cast(),
            )
        };
        let result = if ok == 0 { Err(wsa_error()) } else { Ok(()) };
        if self.reported_later(listener as HANDLE, &result) {
            self.inflight += 1;
            self.multi(key).pending = record;
            return Ok(None);
        }
        if let Err(code) = result {
            // SAFETY: nothing is in flight; dropping it closes the socket.
            drop(unsafe { Box::from_raw(record) });
            return Err(-code);
        }
        // SAFETY: finished inside the call, so the kernel does not hold it.
        let mut record = unsafe { Box::from_raw(record) };
        Self::accepted(&mut record).map(Some)
    }

    /// Turns the record of a successful `AcceptEx` into the accepted
    /// socket, which the caller then owns.
    fn accepted(record: &mut Record) -> Result<i32, i32> {
        let accepting = record.accepting.as_mut().expect("an accept's socket");
        match Self::finish(record.after, accepting.socket) {
            0 => {}
            err => return Err(err),
        }
        // Windows keeps kernel handles within 32 bits; one that is not
        // cannot be reported and is closed with the record.
        let socket = i32::try_from(accepting.socket).map_err(|_| -WSAEINVAL)?;
        accepting.socket = INVALID_SOCKET;
        Ok(socket)
    }

    pub(crate) fn drop_multi(&mut self, key: usize) {
        let Some(Slot::Multi(multi)) = self.ops.remove(key) else {
            return;
        };
        if let Some(socket) = multi.accepted.filter(|&s| s >= 0) {
            // Accepted for an owner that is gone.
            // SAFETY: a socket this driver accepted and still owns.
            unsafe { closesocket(socket as SOCKET) };
        }
        if !multi.pending.is_null() {
            // SAFETY: a pending record is alive until its completion.
            let rec = unsafe { &mut *multi.pending };
            rec.state = State::Orphan(None);
            Self::cancel(rec);
        }
    }

    // -------------------------------------------------------------------- park

    /// Collects what the port has, waiting at most `timeout` milliseconds.
    fn collect(&mut self, timeout: u32) -> usize {
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
                return 0;
            }
            super::event_loop_failed("GetQueuedCompletionStatusEx", err);
        }
        count as usize
    }

    /// `true` if something can be collected without waiting.
    pub(crate) fn has_completions(&mut self) -> bool {
        if self.ready == 0 {
            self.ready = self.collect(0);
        }
        self.ready > 0
    }

    pub(crate) fn park(&mut self, mode: Park) -> Vec<Box<dyn Any>> {
        let mut n = if self.ready > 0 {
            mem::take(&mut self.ready)
        } else {
            let timeout = match mode {
                Park::Poll => 0,
                Park::Wait(None) => u32::MAX, // INFINITE
                Park::Wait(Some(d)) => match self.alarm.as_ref().map(|a| a.set(self.port.0, d)) {
                    Some(alarm::Set::Expired) => 0,
                    // The alarm ends the wait on time. The timeout stays,
                    // rounded up so that it never ends the wait first:
                    // waking before the deadline would spin until it.
                    // Windows may still end a wait early by its clock
                    // tick; the event loop then waits again for what is
                    // left.
                    _ => d
                        .as_nanos()
                        .div_ceil(1_000_000)
                        .min(u128::from(u32::MAX - 1)) as u32,
                },
            };
            self.collect(timeout)
        };
        loop {
            for i in 0..n {
                let entry = self.events[i];
                // A packet without an operation is a wake-up: returning
                // from the wait is all it asks.
                if !entry.lpOverlapped.is_null() {
                    self.complete(entry);
                } else if entry.lpCompletionKey == alarm::KEY {
                    if let Some(alarm) = &self.alarm {
                        alarm.rang();
                    }
                }
            }
            if n < BATCH {
                break;
            }
            // A full batch: more may be queued. Take it without waiting.
            n = self.collect(0);
        }
        mem::take(&mut self.graveyard)
    }

    fn complete(&mut self, entry: OVERLAPPED_ENTRY) {
        let record = entry.lpOverlapped.cast::<Record>();
        // SAFETY: every OVERLAPPED this port reports heads a record made
        // by this driver, alive until this very report.
        let rec = unsafe { &mut *record };
        if let State::Lingering = rec.state {
            // Reported when its call returned; this is the packet that
            // the warning status still queued. It was never counted in
            // flight.
            self.lingering.remove(&(record as usize));
            // SAFETY: reported, so neither the kernel nor the port holds
            // it any more.
            drop(unsafe { Box::from_raw(record) });
            return;
        }
        self.inflight -= 1;
        // `Internal` is the NTSTATUS; 0 is success.
        let res = if rec.overlapped.Internal == 0 {
            entry.dwNumberOfBytesTransferred as i32
        } else {
            let (mut bytes, mut flags) = (0u32, 0u32);
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
            } else if Self::is_end(rec, err) {
                0
            } else if !rec.socket && err == ERROR_MORE_DATA as i32 {
                // Part of a pipe message: what came, as a stream read.
                bytes as i32
            } else {
                -err
            }
        };
        match mem::replace(&mut rec.state, State::Done(res)) {
            State::Waiting(waker) => {
                if res >= 0 {
                    let after = Self::finish(rec.after, INVALID_SOCKET);
                    if after != 0 {
                        rec.state = State::Done(after);
                    }
                } else {
                    rec.state = State::Done(Self::cut(rec.after, res));
                }
                waker.wake();
            }
            State::Orphan(data) => {
                // Nobody waits. The resources go now that the kernel is
                // done with them, outside the driver (`park` returns them).
                self.graveyard.extend(data);
                self.orphans.remove(&(record as usize));
                // SAFETY: reported, so the kernel no longer holds it. A
                // socket accepted for nobody closes with it.
                drop(unsafe { Box::from_raw(record) });
            }
            State::Multi(key) => {
                // SAFETY: as above.
                let mut record = unsafe { Box::from_raw(record) };
                let accepted = record.accepting.is_some().then(|| {
                    if res < 0 {
                        res
                    } else {
                        Self::accepted(&mut record).unwrap_or_else(|e| e)
                    }
                });
                let datagram = record
                    .datagram
                    .take()
                    .map(|mut d| Self::datagram(&mut d, res));
                let multi = self.multi(key);
                multi.pending = ptr::null_mut();
                match datagram {
                    Some(Ok(data)) => multi.data = Some(data),
                    Some(Err(e)) => multi.accepted = Some(e),
                    None => {}
                }
                // A zero-byte receive that failed leaves nothing: the read
                // that follows meets the same error and reports it.
                if let Some(res) = accepted {
                    multi.accepted = Some(res);
                }
                for waker in multi.wakers.drain(..) {
                    waker.wake();
                }
            }
            State::Done(_) | State::Lingering => {
                unreachable!("giong: an operation completed twice")
            }
        }
    }

    /// Cancels what is left and waits for it. `false` means the kernel may
    /// still use the driver's memory: the caller must leak it.
    pub(crate) fn shutdown(&mut self) -> bool {
        // Completions that `has_completions` collected and nobody
        // processed: the waits below would overwrite them.
        for i in 0..mem::take(&mut self.ready) {
            let entry = self.events[i];
            if !entry.lpOverlapped.is_null() {
                self.complete(entry);
            }
        }
        for slot in self.ops.iter() {
            let record = match slot {
                Slot::Single(record) => *record,
                Slot::Multi(multi) => multi.pending,
            };
            if record.is_null() {
                continue;
            }
            // SAFETY: a record in a slot is alive.
            let rec = unsafe { &*record };
            if !matches!(rec.state, State::Done(_)) {
                Self::cancel(rec);
            }
        }
        // Orphans left to finish (a stream send, a job already running):
        // they are not in `ops`.
        for &record in &self.orphans {
            // SAFETY: an orphan is alive until its completion.
            Self::cancel(unsafe { &*(record as *const Record) });
        }
        for _ in 0..100 {
            if self.inflight == 0 {
                break;
            }
            let n = self.collect(10);
            for i in 0..n {
                let entry = self.events[i];
                if !entry.lpOverlapped.is_null() {
                    self.complete(entry);
                }
            }
        }
        self.inflight == 0
    }

    pub(crate) fn take_graveyard(&mut self) -> Vec<Box<dyn Any>> {
        mem::take(&mut self.graveyard)
    }

    /// Operations the kernel has not reported yet.
    pub(crate) fn inflight(&self) -> usize {
        self.inflight
    }

    pub(crate) fn tracked_ops(&self) -> usize {
        self.ops.len()
    }
}

impl Drop for Iocp {
    fn drop(&mut self) {
        // Completed and never collected: nothing holds these but us.
        // Records still in flight are leaked: the kernel may write to them.
        for slot in self.ops.iter() {
            if let Slot::Single(record) = slot {
                // SAFETY: a record in a slot is alive.
                if let State::Done(_) = unsafe { &(**record).state } {
                    // SAFETY: completed, so the kernel no longer holds it.
                    drop(unsafe { Box::from_raw(*record) });
                }
            }
        }
        // Their calls ended long ago; only a packet nobody will read now
        // names them.
        for &record in &self.lingering {
            // SAFETY: the kernel finished with it when its call returned.
            drop(unsafe { Box::from_raw(record as *mut Record) });
        }
    }
}

#[cfg(test)]
mod tests {
    use std::os::windows::io::AsRawSocket;
    use std::time::Duration;

    use super::*;
    use windows_sys::Win32::System::LibraryLoader::{GetModuleHandleA, GetProcAddress};
    use windows_sys::Win32::System::IO::GetQueuedCompletionStatus;

    fn under_wine() -> bool {
        // SAFETY: looks up an export of a module every process has.
        unsafe {
            let ntdll = GetModuleHandleA(c"ntdll.dll".as_ptr().cast());
            !ntdll.is_null() && GetProcAddress(ntdll, c"wine_get_version".as_ptr().cast()).is_some()
        }
    }

    /// A registration that fails leaves the handle out of the port, so a
    /// later registration still works: a handle joined to the port but not
    /// recorded made every later operation on it fail with error 87.
    #[test]
    fn a_failed_registration_leaves_the_handle_free_to_register_again() {
        use std::os::windows::fs::OpenOptionsExt;
        use std::os::windows::io::AsRawHandle;

        let mut iocp = Iocp::new(BufPool::new(4, 1024).unwrap()).unwrap();
        let path = std::env::temp_dir().join(format!("giong-register-{}", std::process::id()));
        let file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .custom_flags(windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OVERLAPPED)
            .open(&path)
            .unwrap();
        let handle = file.as_raw_handle() as HANDLE;
        // A file is no socket: FIONBIO fails, before the port is joined.
        assert!(iocp.register(handle, true).is_err());
        assert!(!iocp.registered.contains_key(&(handle as usize)));
        assert_eq!(iocp.register(handle, false), Ok(()));
        assert!(iocp.registered.contains_key(&(handle as usize)));
        drop(file);
        let _ = std::fs::remove_file(&path);
    }

    /// A file operation whose future is dropped while it waits for a
    /// blocking thread does not run later: a removal queued behind busy
    /// threads would otherwise remove a file made since.
    #[test]
    fn a_queued_file_operation_whose_future_is_dropped_never_runs() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::{Arc, Condvar, Mutex};

        let path = std::env::temp_dir().join(format!("giong-queued-{}", std::process::id()));
        std::fs::write(&path, b"first").unwrap();
        // Every blocking thread busy until `release`.
        let gate = Arc::new((Mutex::new(false), Condvar::new()));
        let started = Arc::new(AtomicUsize::new(0));
        struct Release(Arc<(Mutex<bool>, Condvar)>);
        impl Drop for Release {
            fn drop(&mut self) {
                *self.0 .0.lock().unwrap() = true;
                self.0 .1.notify_all();
            }
        }
        let release = Release(gate.clone());
        for _ in 0..super::super::blocking::MAX_THREADS {
            let (gate, started) = (gate.clone(), started.clone());
            super::super::blocking::spawn(Box::new(move || {
                started.fetch_add(1, Ordering::SeqCst);
                let mut open = gate.0.lock().unwrap();
                while !*open {
                    open = gate.1.wait(open).unwrap();
                }
            }));
        }
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while started.load(Ordering::SeqCst) < super::super::blocking::MAX_THREADS {
            assert!(
                std::time::Instant::now() < deadline,
                "the blocking threads did not all start"
            );
            std::thread::sleep(Duration::from_millis(1));
        }
        let target = path.clone();
        crate::Builder::new()
            .block_on(async move {
                // Polled once, so that it is queued, then dropped at once:
                // a timed wait could outlast the pool's stall check, which
                // starts a thread for a job queued behind stuck ones.
                let mut removal = Box::pin(crate::fs::remove_file(&target));
                use std::future::Future;
                let polled =
                    std::future::poll_fn(|cx| std::task::Poll::Ready(removal.as_mut().poll(cx)))
                        .await;
                assert!(
                    polled.is_pending(),
                    "the removal ran with every thread busy: {polled:?}"
                );
                drop(removal);
                std::fs::write(&target, b"second").unwrap();
                drop(release);
                // Time for the queued job to be taken, had it not been cancelled.
                crate::time::sleep(Duration::from_millis(300)).await;
            })
            .unwrap();
        let kept = std::fs::read(&path);
        let _ = std::fs::remove_file(&path);
        assert_eq!(kept.unwrap(), b"second", "the dropped removal ran later");
    }

    /// What `State::Lingering` is for: a datagram receive that ends inside
    /// the call, cut off (`WSAEMSGSIZE`), still queues a completion packet
    /// with skip-on-success set. Wine queues none, so it is not asked there.
    #[test]
    fn a_receive_cut_off_inside_the_call_still_reports_through_the_port() {
        if under_wine() {
            return;
        }
        let rx = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        let tx = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        tx.send_to(&[7u8; 2000], rx.local_addr().unwrap()).unwrap();
        std::thread::sleep(Duration::from_millis(50));
        let socket = rx.as_raw_socket() as SOCKET;
        // SAFETY: a new port, and a socket this test owns associated with
        // it; the buffer and OVERLAPPED outlive the wait below.
        unsafe {
            let port = CreateIoCompletionPort(INVALID_HANDLE_VALUE, ptr::null_mut(), 0, 1);
            assert!(!port.is_null());
            assert!(!CreateIoCompletionPort(socket as HANDLE, port, 0, 0).is_null());
            let modes = FILE_SKIP_SET_EVENT_ON_HANDLE | FILE_SKIP_COMPLETION_PORT_ON_SUCCESS;
            assert_ne!(
                SetFileCompletionNotificationModes(socket as HANDLE, modes),
                0
            );
            // Non-blocking, as the driver's sockets are.
            let mut on = 1u32;
            assert_eq!(ioctlsocket(socket, FIONBIO, &mut on), 0);
            let mut buf = [0u8; 512];
            let wsabuf = WSABUF {
                len: 512,
                buf: buf.as_mut_ptr(),
            };
            let mut overlapped: Box<OVERLAPPED> = Box::new(mem::zeroed());
            let (mut n, mut flags) = (0u32, 0u32);
            let rc = WSARecv(
                socket,
                &wsabuf,
                1,
                &mut n,
                &mut flags,
                &mut *overlapped,
                None,
            );
            // With the datagram there, the call ends inside, cut off; or
            // Windows leaves it in flight. Either way a packet must come.
            let err = wsa_error();
            assert_eq!(rc, -1);
            eprintln!(
                "the receive {}",
                if err == WSAEMSGSIZE {
                    "ended inside the call, cut off"
                } else {
                    "was left in flight"
                }
            );
            assert!(
                err == WSAEMSGSIZE || err == ERROR_IO_PENDING as i32,
                "error {err}"
            );
            let (mut bytes, mut key, mut got) = (0u32, 0usize, ptr::null_mut());
            let ok = GetQueuedCompletionStatus(port, &mut bytes, &mut key, &mut got, 2000);
            let reported = !got.is_null();
            assert!(
                ok != 0 || reported,
                "no packet came for the receive that ended with {err} (error {})",
                last_error()
            );
            assert_eq!(got, &mut *overlapped as *mut OVERLAPPED);
            CloseHandle(port);
        }
    }
}
