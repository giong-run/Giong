//! One listening port for several workers, on Windows.
//!
//! Windows has no `SO_REUSEPORT`: a port has one listening socket, and a
//! socket reports its completions to one completion port, so to one worker.
//! Every worker can still bind its own [`TcpListener`](super::TcpListener)
//! to the same address:
//!
//! - The first listener of an address binds the socket and accepts on it
//!   through its worker's driver, as a listener nobody shares with does.
//! - When a second listener binds the address, it gets a handle to the same
//!   socket, and from then on one thread takes the connections off the
//!   socket and hands them to the listeners in turn. Each listener's
//!   `accept` returns the connections handed to it.
//!
//! The thread takes a connection only while some listener has room for it;
//! otherwise connections wait in the kernel's backlog, as they do when
//! nobody accepts. A listener that goes away gives the connections waiting
//! for it to the others. The thread ends with the last listener.

use std::collections::{HashMap, VecDeque};
use std::io;
use std::net::SocketAddr;
use std::os::windows::io::{AsRawSocket, FromRawSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};
use std::thread::JoinHandle;

use socket2::Socket;

use super::ListenSocket;
use crate::runtime::lock;
use crate::sys::{accept_nowait, AcceptEvents, AcceptWake, RawSock};

/// Connections that may wait for one listener before the thread stops
/// taking more for it: the backlog a listener asks the kernel for.
const INBOX_MAX: usize = 4096;

/// The addresses that listeners of this process share.
static SPREADS: Mutex<Option<HashMap<SocketAddr, Arc<Spread>>>> = Mutex::new(None);

/// One shared address.
struct Spread {
    addr: SocketAddr,
    /// A handle to the listening socket that lives as long as any listener,
    /// whichever of them goes first.
    master: Socket,
    /// Set once a second listener joined: the thread accepts from then on.
    dispatching: AtomicBool,
    state: Mutex<State>,
}

struct State {
    inboxes: Vec<Arc<Inbox>>,
    /// Where the next connection goes, so that they go round.
    next: usize,
    dispatcher: Option<(Arc<AcceptEvents>, JoinHandle<()>)>,
}

/// The connections handed to one listener.
#[derive(Default)]
struct Inbox {
    slot: Mutex<Slot>,
}

#[derive(Default)]
struct Slot {
    sockets: VecDeque<Socket>,
    waiter: Option<Waker>,
    /// `false` once the listener is gone.
    open: bool,
}

/// One listener's share of an address.
pub(crate) struct Member {
    spread: Arc<Spread>,
    inbox: Arc<Inbox>,
    /// The listener that bound the socket accepts on it by itself until
    /// another one joins.
    first: bool,
}

/// A listening socket for `addr` and its share of the address.
///
/// If listeners of this process hold the address already, the socket is a
/// handle to theirs. Otherwise `bind` makes it, bound and listening, and
/// later listeners join it; what else `bind` returns is handed on. The
/// registry is locked from the look to the entry, so that of two workers
/// that bind an address at the same moment one binds and the other joins.
pub(crate) fn bind<T>(
    addr: SocketAddr,
    bind: impl FnOnce() -> io::Result<(Socket, T)>,
) -> io::Result<(Socket, Option<T>, Option<Member>)> {
    let mut spreads = lock(&SPREADS);
    let known = match addr.port() {
        0 => None,
        _ => spreads.as_ref().and_then(|s| s.get(&addr)).cloned(),
    };
    let Some(spread) = known else {
        let (sock, other) = bind()?;
        let member = first(spreads.get_or_insert_with(HashMap::new), &sock);
        return Ok((sock, Some(other), member));
    };
    let sock = spread.master.try_clone()?;
    let inbox = Arc::new(Inbox::default());
    lock(&inbox.slot).open = true;
    {
        let mut state = lock(&spread.state);
        if state.dispatcher.is_none() {
            let events = Arc::new(AcceptEvents::new(spread.master.as_raw_socket() as RawSock)?);
            let (for_thread, shared) = (events.clone(), spread.clone());
            let thread = std::thread::Builder::new()
                .name("giong-accept".into())
                .spawn(move || shared.dispatch(&for_thread))?;
            state.dispatcher = Some((events, thread));
            spread.dispatching.store(true, Ordering::Release);
            // The first listener may be waiting in its own accept: it has
            // to look again to see that the thread took over.
            for inbox in &state.inboxes {
                if let Some(waker) = lock(&inbox.slot).waiter.take() {
                    waker.wake();
                }
            }
        }
        state.inboxes.push(inbox.clone());
    }
    drop(spreads);
    let member = Member {
        spread,
        inbox,
        first: false,
    };
    Ok((sock, None, Some(member)))
}

/// Makes the address `sock` was just bound to one that later listeners
/// join. `None` if the socket's address cannot be read or duplicated, and
/// the listener then keeps the port to itself.
fn first(spreads: &mut HashMap<SocketAddr, Arc<Spread>>, sock: &Socket) -> Option<Member> {
    let addr = sock.local_addr().ok()?.as_socket()?;
    let master = sock.try_clone().ok()?;
    let inbox = Arc::new(Inbox::default());
    lock(&inbox.slot).open = true;
    let spread = Arc::new(Spread {
        addr,
        master,
        dispatching: AtomicBool::new(false),
        state: Mutex::new(State {
            inboxes: vec![inbox.clone()],
            next: 0,
            dispatcher: None,
        }),
    });
    spreads.insert(addr, spread.clone());
    Some(Member {
        spread,
        inbox,
        first: true,
    })
}

impl Member {
    /// The next connection for this listener.
    pub(crate) fn poll_accept(
        &self,
        cx: &mut Context<'_>,
        listener: &ListenSocket,
    ) -> Poll<io::Result<Socket>> {
        {
            let mut slot = lock(&self.inbox.slot);
            if let Some(sock) = slot.sockets.pop_front() {
                return Poll::Ready(Ok(sock));
            }
            match &slot.waiter {
                Some(w) if w.will_wake(cx.waker()) => {}
                _ => slot.waiter = Some(cx.waker().clone()),
            }
        }
        if !self.first {
            return Poll::Pending;
        }
        if !self.spread.dispatching.load(Ordering::Acquire) {
            return listener.poll_accept(cx);
        }
        // The thread accepts from now on. The accept this listener may
        // still have in the driver takes one more connection, which is
        // this listener's; it starts no other, or it would take most
        // connections ahead of the thread.
        listener.poll_last_accept(cx)
    }
}

impl Spread {
    /// The thread: takes connections off the socket and hands them out,
    /// until it is told to stop.
    fn dispatch(&self, events: &AcceptEvents) {
        let listener = self.master.as_raw_socket() as RawSock;
        let mut wait = None;
        loop {
            match events.wait(wait) {
                AcceptWake::Stop => return,
                AcceptWake::Accept | AcceptWake::Timeout => {}
            }
            wait = None;
            loop {
                // A place first, then the connection: one that nobody has
                // room for stays in the kernel's backlog.
                let Some(inbox) = self.next_inbox() else {
                    wait = Some(1);
                    break;
                };
                match accept_nowait(listener) {
                    // SAFETY: a new socket that nothing else owns.
                    Ok(Some(raw)) => self.hand(inbox, unsafe { Socket::from_raw_socket(raw as _) }),
                    Ok(None) => break,
                    // A connection that failed before it was accepted, or
                    // no memory or handles for one right now: try again
                    // shortly rather than spin.
                    Err(_) => {
                        wait = Some(1);
                        break;
                    }
                }
            }
        }
    }

    /// An open inbox with room, the next in turn.
    fn next_inbox(&self) -> Option<Arc<Inbox>> {
        let mut state = lock(&self.state);
        let count = state.inboxes.len();
        for step in 0..count {
            let at = (state.next + step) % count;
            let inbox = &state.inboxes[at];
            let slot = lock(&inbox.slot);
            if slot.open && slot.sockets.len() < INBOX_MAX {
                drop(slot);
                let inbox = inbox.clone();
                state.next = at + 1;
                return Some(inbox);
            }
        }
        None
    }

    /// Gives `sock` to `inbox`, or to another one if its listener went
    /// away meanwhile; closes it if every listener did.
    fn hand(&self, inbox: Arc<Inbox>, sock: Socket) {
        let mut target = Some(inbox);
        let mut sock = Some(sock);
        while let (Some(inbox), Some(s)) = (target.take(), sock.take()) {
            let mut slot = lock(&inbox.slot);
            if slot.open {
                slot.sockets.push_back(s);
                let waiter = slot.waiter.take();
                drop(slot);
                if let Some(waker) = waiter {
                    waker.wake();
                }
                return;
            }
            drop(slot);
            sock = Some(s);
            target = self.next_inbox();
        }
    }
}

impl Drop for Member {
    fn drop(&mut self) {
        let spread = &self.spread;
        let waiting = {
            let mut slot = lock(&self.inbox.slot);
            slot.open = false;
            slot.waiter = None;
            std::mem::take(&mut slot.sockets)
        };
        // The registry first, then the state: the order `join` takes them.
        let mut spreads = lock(&SPREADS);
        let dispatcher = {
            let mut state = lock(&spread.state);
            state.inboxes.retain(|i| !Arc::ptr_eq(i, &self.inbox));
            if state.inboxes.is_empty() {
                if let Some(map) = spreads.as_mut() {
                    map.remove(&spread.addr);
                }
                state.dispatcher.take()
            } else {
                None
            }
        };
        drop(spreads);
        for sock in waiting {
            match spread.next_inbox() {
                Some(inbox) => spread.hand(inbox, sock),
                None => drop(sock),
            }
        }
        if let Some((events, thread)) = dispatcher {
            // The last listener: the thread ends, and with it its use of
            // the socket, before this returns, so that the port is free
            // once the listener is dropped.
            events.stop();
            drop(thread.join());
        }
    }
}
