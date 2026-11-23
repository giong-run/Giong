//! Datagrams that reach the wrong worker go to the one that owns their
//! connection.
//!
//! Every server worker binds its own socket to the same address, and the
//! kernel picks the socket by the sender's address. A client whose address
//! changes (NAT rebinding, Wi-Fi to cellular) can so land on a worker that
//! does not know its connection. The connection IDs a giong endpoint
//! issues name their worker ([`crate::native::cid`]), which is enough to
//! pass the datagram on, with no table shared between workers.
//!
//! Only packets with a short header are passed on: a client may not move
//! before the handshake is confirmed (RFC 9000, section 9), and the
//! destination connection ID of a first Initial packet is the client's
//! choice, which names no one.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};

use giong::sync::mpsc::{bounded, Receiver, Sender, TrySendError};
use quinn_proto::{ConnectionId, ConnectionIdGenerator, InvalidCid};

use crate::native::cid::{self, owner};

/// Datagrams one worker holds for another before it drops new ones, as a
/// full socket buffer would.
const QUEUE: usize = 1024;

/// A datagram on its way to the worker that owns its connection.
pub(crate) struct Forwarded {
    pub(crate) data: Vec<u8>,
    pub(crate) from: SocketAddr,
    pub(crate) ecn: u8,
    pub(crate) segment: Option<usize>,
    /// The local address it was sent to, on a wildcard bind.
    pub(crate) local: Option<std::net::IpAddr>,
}

/// The owner of every datagram of a receive that holds several (`segment`
/// bytes each, the last one maybe shorter), when they all have the same.
pub(crate) fn owner_of_all(data: &[u8], segment: usize, me: u16) -> Result<Option<u16>, Mixed> {
    let mut chunks = data.chunks(segment.max(1));
    let first = chunks.next().and_then(|d| owner(d, me));
    if chunks.all(|d| owner(d, me) == first) {
        Ok(first)
    } else {
        Err(Mixed)
    }
}

/// The datagrams of one receive do not all go to the same worker.
#[derive(Debug, PartialEq)]
pub(crate) struct Mixed;

/// The endpoints bound to one address, by worker. A worker normally has
/// one; a second one bound on the same worker (a service that binds the
/// new endpoint before it drops the old) waits behind the first, and takes
/// its place when the first is dropped.
type Group = HashMap<u16, Vec<(u64, Sender<Forwarded>)>>;

fn groups() -> &'static Mutex<HashMap<SocketAddr, Group>> {
    static GROUPS: OnceLock<Mutex<HashMap<SocketAddr, Group>>> = OnceLock::new();
    GROUPS.get_or_init(Default::default)
}

/// Goes up whenever an endpoint joins or leaves, so that the others look
/// their peers up again.
static GENERATION: AtomicU64 = AtomicU64::new(0);

/// Names every member, so that one can leave its group's list.
static NEXT_ID: AtomicU64 = AtomicU64::new(0);

/// A server endpoint's place among the endpoints bound to its address.
pub(crate) struct Member {
    addr: SocketAddr,
    worker: u16,
    id: u64,
    seen: u64,
    peers: HashMap<u16, Sender<Forwarded>>,
    /// The workers of the group (the peers and this one), sorted; kept
    /// with `peers` so that an Initial packet allocates nothing.
    workers: Vec<u16>,
}

impl Member {
    /// Joins the endpoints bound to `addr`; gives where the datagrams the
    /// others pass on arrive.
    pub(crate) fn join(addr: SocketAddr, worker: u16) -> (Self, Receiver<Forwarded>) {
        let (tx, rx) = bounded(QUEUE);
        let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
        let mut groups = groups().lock().unwrap_or_else(|e| e.into_inner());
        groups
            .entry(addr)
            .or_default()
            .entry(worker)
            .or_default()
            .push((id, tx));
        GENERATION.fetch_add(1, Ordering::Release);
        let member = Member {
            addr,
            worker,
            id,
            // Looked up at the first datagram that needs it.
            seen: u64::MAX,
            peers: HashMap::new(),
            workers: Vec::new(),
        };
        (member, rx)
    }

    pub(crate) fn worker(&self) -> u16 {
        self.worker
    }

    /// Passes a datagram on to `worker`. Gives it back if no endpoint of
    /// that worker is bound to this address (a connection ID from before a
    /// restart, or bytes that only look like one): the caller answers it
    /// as it answers any packet it does not know. A datagram for a worker
    /// whose queue is full is dropped, as the network drops.
    pub(crate) fn forward(&mut self, worker: u16, datagram: Forwarded) -> Option<Forwarded> {
        self.refresh();
        match self.peers.get(&worker) {
            Some(peer) => match peer.try_send(datagram) {
                Ok(()) | Err(TrySendError::Full(_)) => None,
                Err(TrySendError::Closed(datagram)) => Some(datagram),
            },
            None => Some(datagram),
        }
    }
}

impl Member {
    /// Looks the other endpoints of the address up again if one joined or
    /// left since the last time.
    fn refresh(&mut self) {
        let now = GENERATION.load(Ordering::Acquire);
        if self.seen != now {
            let groups = groups().lock().unwrap_or_else(|e| e.into_inner());
            self.peers = groups
                .get(&self.addr)
                .into_iter()
                .flatten()
                .filter(|(&worker, _)| worker != self.worker)
                .filter_map(|(&worker, list)| Some((worker, list.first()?.1.clone())))
                .collect();
            self.workers.clear();
            self.workers.extend(self.peers.keys().copied());
            self.workers.push(self.worker);
            self.workers.sort_unstable();
            self.seen = now;
        }
    }

    /// The worker a packet with a long header belongs to, when it is
    /// another than this one, where the kernel does not keep a client's
    /// datagrams on one worker: on macOS the workers' sockets are one
    /// socket, and each datagram goes to the worker that reads first. A
    /// connection ID one of the workers issued names its worker; one the
    /// client chose (its first Initial packets, and its 0-RTT ones) is
    /// hashed to a worker of the address, the same on every worker, so
    /// that a first flight of several datagrams meets in one place.
    ///
    /// Right after a worker joins or leaves, workers can briefly see
    /// different groups and choose different owners. A handshake split
    /// that way is not lost: the client sends its Initial again, and by
    /// then every worker sees the same group. A forwarded datagram is
    /// handled where it arrives and never forwarded again.
    #[cfg_attr(target_os = "linux", allow(dead_code))]
    pub(crate) fn long_owner(&mut self, datagram: &[u8]) -> Option<u16> {
        let dcid = cid::long_dcid(datagram)?;
        self.refresh();
        let named =
            cid::worker_of(dcid).filter(|w| *w == self.worker || self.peers.contains_key(w));
        let worker = match named {
            Some(worker) => worker,
            None => {
                let workers = &self.workers;
                // FNV-1a: the same on every worker, and enough to spread
                // the IDs clients pick at random.
                let hash = dcid.iter().fold(0xcbf2_9ce4_8422_2325u64, |h, &b| {
                    (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3)
                });
                workers[(hash % workers.len() as u64) as usize]
            }
        };
        (worker != self.worker).then_some(worker)
    }
}

impl Drop for Member {
    fn drop(&mut self) {
        let mut groups = groups().lock().unwrap_or_else(|e| e.into_inner());
        if let Some(group) = groups.get_mut(&self.addr) {
            if let Some(list) = group.get_mut(&self.worker) {
                list.retain(|(id, _)| *id != self.id);
                if list.is_empty() {
                    group.remove(&self.worker);
                }
            }
            if group.is_empty() {
                groups.remove(&self.addr);
            }
        }
        GENERATION.fetch_add(1, Ordering::Release);
    }
}

/// Connection IDs that name the worker they were issued on.
pub(crate) struct WorkerCids {
    pub(crate) worker: u16,
}

impl ConnectionIdGenerator for WorkerCids {
    fn generate_cid(&mut self) -> ConnectionId {
        let mut random = [0; cid::LEN];
        rustls::crypto::ring::default_provider()
            .secure_random
            .fill(&mut random)
            .expect("the system's random generator");
        ConnectionId::new(&cid::issue(self.worker, random))
    }

    // Any connection ID laid out as giong issues them, whichever worker it
    // names: quinn-proto drops a packet whose ID does not validate before
    // it would answer with a stateless reset, and a packet comes here for
    // another worker only when that worker has no endpoint (after a
    // restart with fewer workers, say), which is when the client should
    // be told its connection is gone.
    fn validate(&self, id: &ConnectionId) -> Result<(), InvalidCid> {
        if cid::worker_of(id).is_some() {
            Ok(())
        } else {
            Err(InvalidCid)
        }
    }

    fn cid_len(&self) -> usize {
        cid::LEN
    }

    fn cid_lifetime(&self) -> Option<std::time::Duration> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    fn short(worker: u16) -> Vec<u8> {
        let mut d = vec![0x40];
        d.extend_from_slice(&cid::issue(worker, [0xab; 8]));
        d.extend_from_slice(&[0; 20]);
        d
    }

    fn datagram() -> Forwarded {
        Forwarded {
            data: vec![1],
            from: "127.0.0.1:1".parse().unwrap(),
            ecn: 0,
            segment: None,
            local: None,
        }
    }

    #[test]
    fn a_second_endpoint_on_a_worker_takes_over_when_the_first_goes() {
        let addr: SocketAddr = "127.0.0.1:9".parse().unwrap();
        let (mut sender, _) = Member::join(addr, 0);
        let (old, mut old_rx) = Member::join(addr, 1);
        let (new, mut new_rx) = Member::join(addr, 1);
        // The first endpoint of worker 1 gets what worker 0 passes on.
        assert!(sender.forward(1, datagram()).is_none());
        assert!(old_rx.try_recv().is_ok());
        assert!(new_rx.try_recv().is_err());
        // Once it is gone, the second one does.
        drop((old, old_rx));
        assert!(sender.forward(1, datagram()).is_none());
        assert!(new_rx.try_recv().is_ok());
        // And with none left, the datagram comes back.
        drop((new, new_rx));
        assert!(sender.forward(1, datagram()).is_some());
    }

    fn long(dcid: &[u8]) -> Vec<u8> {
        let mut d = vec![0xc0, 0, 0, 0, 1, dcid.len() as u8];
        d.extend_from_slice(dcid);
        d.extend_from_slice(&[0; 20]);
        d
    }

    #[test]
    fn long_headers_meet_on_one_worker_whichever_reads_them() {
        let addr: SocketAddr = "127.0.0.1:7".parse().unwrap();
        let mut members: Vec<_> = (0..3).map(|w| Member::join(addr, w)).collect();
        // An ID one of them issued goes to its worker.
        let issued = long(&cid::issue(2, [5; 8]));
        let owners: Vec<_> = members
            .iter_mut()
            .map(|(m, _)| m.long_owner(&issued))
            .collect();
        assert_eq!(owners, [Some(2), Some(2), None]);
        // One the client chose, or one that names a worker the address
        // does not have: every worker sends it to the same one.
        for dcid in [
            &[0xff, 1, 2, 3, 4, 5, 6, 7][..],
            &cid::issue(9, [1; 8]),
            &[7; 20],
        ] {
            let datagram = long(dcid);
            let picked: Vec<_> = members
                .iter_mut()
                .map(|(m, _)| m.long_owner(&datagram).unwrap_or(m.worker()))
                .collect();
            assert!(picked.iter().all(|w| *w == picked[0]), "{picked:?}");
            assert!(picked[0] < 3);
        }
        // Spread over the workers, not all to one.
        let firsts: HashSet<_> = (0..64u8)
            .map(|i| {
                let (m, _) = &mut members[0];
                m.long_owner(&long(&[i, 0xee, 0, 0, 0, 0, 0, 1]))
                    .unwrap_or(0)
            })
            .collect();
        assert_eq!(firsts.len(), 3);
        // A short header is not one of these.
        assert_eq!(members[0].0.long_owner(&short(1)), None);
    }

    #[test]
    fn only_short_headers_of_another_worker_are_passed_on() {
        assert_eq!(owner(&short(3), 1), Some(3));
        assert_eq!(owner(&short(3), 3), None);
        // A long header keeps the worker the kernel chose.
        let mut long = short(3);
        long[0] = 0xc0;
        assert_eq!(owner(&long, 1), None);
        // Without the fixed bit, as a peer that greases it sends.
        let mut unfixed = short(3);
        unfixed[0] = 0x00;
        assert_eq!(owner(&unfixed, 1), Some(3));
        // Too short, and an ID that is not one of ours.
        assert_eq!(owner(&short(3)[..8], 1), None);
        assert_eq!(owner(&[], 1), None);
        let mut other = short(3);
        other[1] |= 0xc0;
        assert_eq!(owner(&other, 1), None);
    }

    #[test]
    fn a_receive_of_several_datagrams_goes_as_one_when_they_agree() {
        let one = short(2);
        let mut all = one.repeat(3);
        all.extend_from_slice(&one[..12]);
        assert_eq!(owner_of_all(&all, one.len(), 1), Ok(Some(2)));
        assert_eq!(owner_of_all(&all, one.len(), 2), Ok(None));
        let mut mixed = one.repeat(2);
        mixed.extend_from_slice(&short(5));
        assert_eq!(owner_of_all(&mixed, one.len(), 1), Err(Mixed));
        assert_eq!(owner_of_all(&[], 0, 1), Ok(None));
    }

    #[test]
    fn ids_name_their_worker_and_any_giong_id_validates() {
        let mut cids = WorkerCids { worker: 9 };
        let id = cids.generate_cid();
        assert_eq!(id.len(), cid::LEN);
        assert_eq!(cid::worker_of(&id), Some(9));
        assert!(cids.validate(&id).is_ok());
        // Another worker's: valid, so that it can be answered with a
        // stateless reset when that worker is gone.
        assert!(WorkerCids { worker: 8 }.validate(&id).is_ok());
        // Not laid out as giong issues them.
        assert!(cids.validate(&ConnectionId::new(&[0; 7])).is_err());
        assert_ne!(id, cids.generate_cid());
    }

    #[test]
    fn members_reach_each_other_until_they_leave() {
        let addr: SocketAddr = "127.0.0.1:1".parse().unwrap();
        let datagram = || Forwarded {
            data: vec![1],
            from: addr,
            ecn: 0,
            segment: None,
            local: None,
        };
        let (mut a, _a_in) = Member::join(addr, 0);
        // No one there yet: it comes back.
        assert!(a.forward(1, datagram()).is_some());
        let (b, mut b_in) = Member::join(addr, 1);
        assert!(a.forward(1, datagram()).is_none());
        assert_eq!(b_in.try_recv().unwrap().data, [1]);
        // A second endpoint of the same worker does not take its place.
        let (c, _c_in) = Member::join(addr, 1);
        drop(c);
        assert!(a.forward(1, datagram()).is_none());
        assert!(b_in.try_recv().is_ok());
        // A full queue drops.
        for _ in 0..QUEUE + 10 {
            assert!(a.forward(1, datagram()).is_none());
        }
        assert_eq!(b_in.len(), QUEUE);
        drop(b);
        assert!(a.forward(1, datagram()).is_some());
    }
}
