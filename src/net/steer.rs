//! Connection steering by CPU: a new TCP connection goes to the listener of
//! the worker pinned to the CPU on which the kernel received its SYN.
//!
//! Every worker binds its own listener to a shared port (see `share`), and by
//! default the kernel picks a listener from a hash of the connection's
//! addresses. The CPU that runs the network receive path for a connection
//! (the one its NIC queue interrupts, or that RPS picks) is then usually not
//! the CPU of the worker that serves it: each packet is received on one core,
//! woken on another, and its socket's cache lines move between them.
//!
//! With steering on, a classic BPF program attached to the port's
//! `SO_REUSEPORT` group maps the receiving CPU to the index of the listener
//! whose worker is pinned there. A CPU with no worker returns an index past
//! the end of the group, and the kernel falls back to its hash. Classic BPF
//! needs no privilege.
//!
//! The kernel numbers the members of a group in the order they called
//! listen(2), and when one leaves, moves the last member into its place. The
//! table here mirrors that: listen and leave both happen under its lock.
//!
//! Prototype: turned on with `GIONG_STEER=cpu` in the environment. Whether
//! it becomes a builder option or the default depends on the measurements in
//! docs/reviews/2026-11-breakthrough.md.

use std::collections::BTreeMap;
use std::io;
use std::mem;
use std::net::SocketAddr;
use std::os::fd::AsRawFd;
use std::sync::{Mutex, MutexGuard, OnceLock, PoisonError};

use socket2::Socket;

/// One listener in a steered group: an id that outlives index moves, and the
/// CPU its worker is pinned to.
#[derive(Clone, Copy, Debug)]
struct Member {
    id: u64,
    cpu: Option<u32>,
}

#[derive(Default)]
struct Groups {
    next_id: u64,
    by_addr: BTreeMap<SocketAddr, Vec<Member>>,
}

static GROUPS: Mutex<Groups> = Mutex::new(Groups {
    next_id: 0,
    by_addr: BTreeMap::new(),
});

fn groups() -> MutexGuard<'static, Groups> {
    GROUPS.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Whether steering is on for this process.
pub(crate) fn enabled() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var("GIONG_STEER").is_ok_and(|v| v == "cpu"))
}

/// A listener's place in its steered group, given up with
/// [`leave`](Self::leave) before the socket stops listening.
#[derive(Debug)]
pub(crate) struct Steered {
    addr: SocketAddr,
    id: u64,
}

/// Starts `sock` listening and adds it to the steered group of its address,
/// then attaches the group's new program. The program is attached only when
/// at least one member is pinned; an error attaching it is ignored (the
/// kernel's hash keeps working).
pub(crate) fn listen(sock: &Socket, backlog: i32) -> io::Result<Steered> {
    let addr = sock
        .local_addr()?
        .as_socket()
        .ok_or_else(|| io::Error::from_raw_os_error(libc::EAFNOSUPPORT))?;
    let mut groups = groups();
    sock.listen(backlog)?;
    let id = groups.next_id;
    groups.next_id += 1;
    let members = groups.by_addr.entry(addr).or_default();
    members.push(Member {
        id,
        cpu: pinned_cpu(),
    });
    drop(attach(sock, members));
    Ok(Steered { addr, id })
}

impl Steered {
    /// Takes the listener out of its group's table the way the kernel will
    /// when the socket stops listening (the last member moves into its
    /// place), and attaches the program for what is left through `sock`,
    /// which is still a member until then.
    pub(crate) fn leave(self, sock: &Socket) {
        let mut groups = groups();
        let Some(members) = groups.by_addr.get_mut(&self.addr) else {
            return;
        };
        if let Some(i) = members.iter().position(|m| m.id == self.id) {
            members.swap_remove(i);
        }
        if members.is_empty() {
            groups.by_addr.remove(&self.addr);
            return;
        }
        drop(attach(sock, members));
    }
}

/// The CPU the calling thread is pinned to, when its affinity mask holds
/// exactly one.
fn pinned_cpu() -> Option<u32> {
    // SAFETY: `set` is a valid, zeroed cpu_set_t of the size passed.
    unsafe {
        let mut set: libc::cpu_set_t = mem::zeroed();
        if libc::sched_getaffinity(0, mem::size_of::<libc::cpu_set_t>(), &mut set) != 0 {
            return None;
        }
        let mut found = None;
        for cpu in 0..libc::CPU_SETSIZE as usize {
            if libc::CPU_ISSET(cpu, &set) {
                if found.is_some() {
                    return None;
                }
                found = Some(cpu as u32);
            }
        }
        found
    }
}

/// The classic BPF program for `members`: load the receiving CPU, compare it
/// with each pinned member's CPU, return that member's index; otherwise
/// return an index past the end so the kernel uses its hash.
fn program(members: &[Member]) -> Vec<libc::sock_filter> {
    const SKF_AD_OFF: i32 = -0x1000;
    const SKF_AD_CPU: i32 = 36;
    let ins = |code: u32, jt: u8, jf: u8, k: u32| libc::sock_filter {
        code: code as u16,
        jt,
        jf,
        k,
    };
    let mut prog = vec![ins(
        libc::BPF_LD | libc::BPF_W | libc::BPF_ABS,
        0,
        0,
        (SKF_AD_OFF + SKF_AD_CPU) as u32,
    )];
    for (index, member) in members.iter().enumerate() {
        if let Some(cpu) = member.cpu {
            prog.push(ins(libc::BPF_JMP | libc::BPF_JEQ | libc::BPF_K, 0, 1, cpu));
            prog.push(ins(libc::BPF_RET | libc::BPF_K, 0, 0, index as u32));
        }
    }
    prog.push(ins(libc::BPF_RET | libc::BPF_K, 0, 0, u32::MAX));
    prog
}

fn attach(sock: &Socket, members: &[Member]) -> io::Result<()> {
    if members.iter().all(|m| m.cpu.is_none()) {
        return Ok(());
    }
    let prog = program(members);
    let fprog = libc::sock_fprog {
        len: prog.len() as u16,
        filter: prog.as_ptr() as *mut libc::sock_filter,
    };
    // SAFETY: `fprog` points at `prog`, which outlives the call; the kernel
    // copies the program.
    let rc = unsafe {
        libc::setsockopt(
            sock.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_ATTACH_REUSEPORT_CBPF,
            (&fprog as *const libc::sock_fprog).cast(),
            mem::size_of::<libc::sock_fprog>() as libc::socklen_t,
        )
    };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_program_maps_each_pinned_cpu_to_its_index_and_falls_back_to_the_hash() {
        let members = [
            Member {
                id: 0,
                cpu: Some(3),
            },
            Member { id: 1, cpu: None },
            Member {
                id: 2,
                cpu: Some(5),
            },
        ];
        let prog = program(&members);
        // load, (jeq 3, ret 0), (jeq 5, ret 2), ret past-the-end
        assert_eq!(prog.len(), 6);
        assert_eq!((prog[1].k, prog[2].k), (3, 0));
        assert_eq!((prog[3].k, prog[4].k), (5, 2));
        assert_eq!(prog[5].k, u32::MAX);
    }
}
