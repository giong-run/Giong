//! Connection IDs that name the worker owning the connection.
//!
//! The kernel spreads datagrams over the workers' sockets by a hash of the
//! sender's address. When a client changes address (NAT rebinding, moving
//! from Wi-Fi to cellular), its packets can land on another worker. Every
//! connection ID a server worker issues carries that worker's index at a
//! fixed place, so the packet can be steered back without a shared table:
//! first by forwarding between workers, later by a `SO_REUSEPORT` eBPF
//! program that reads the same bytes (see DESIGN.md).
//!
//! Layout of the 8-byte IDs issued here (in the spirit of QUIC-LB,
//! draft-ietf-quic-load-balancers): byte 0 holds 2 bits of configuration
//! rotation and 6 random bits; bytes 1-2 the worker index, big-endian;
//! bytes 3-7 random. The worker index is visible on the wire: what it tells
//! an observer is which core of the server, not which client.

/// Length of the connection IDs a giong server issues.
pub const LEN: usize = 8;

/// The rotation codepoint of the IDs issued here. `0b11` is reserved by
/// QUIC-LB for IDs that carry no routing information.
pub const ROTATION: u8 = 0;

/// A connection ID for `worker`, filled from `random` (8 bytes from a
/// cryptographic generator).
pub fn issue(worker: u16, random: [u8; LEN]) -> [u8; LEN] {
    let mut id = random;
    id[0] = (ROTATION << 6) | (random[0] & 0x3f);
    id[1..3].copy_from_slice(&worker.to_be_bytes());
    id
}

/// The worker that issued `dcid`, if it is one of ours.
pub fn worker_of(dcid: &[u8]) -> Option<u16> {
    if dcid.len() != LEN || dcid[0] >> 6 != ROTATION {
        return None;
    }
    Some(u16::from_be_bytes([dcid[1], dcid[2]]))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_worker_comes_back_from_the_id() {
        for (w, r) in [(0u16, [0xff; 8]), (7, [0x12; 8]), (u16::MAX, [0; 8])] {
            let id = issue(w, r);
            assert_eq!(worker_of(&id), Some(w));
            assert_eq!(id[0] >> 6, ROTATION);
            assert_eq!(&id[3..], &r[3..]);
        }
        assert_eq!(worker_of(&[0; 7]), None);
        assert_eq!(worker_of(&[0xc0, 0, 1, 0, 0, 0, 0, 0]), None);
    }
}
