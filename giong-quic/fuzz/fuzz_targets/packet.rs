//! The native engine's packet parsing, which every datagram from the
//! network reaches before any authentication, the choice of the worker
//! a datagram belongs to, and the scan of the ClientHello: never panic, and whatever a header claims stays inside the
//! datagram.
#![no_main]

use giong_quic::native::hello::{self, Params};
use giong_quic::native::{cid, packet, varint};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let cid_len = data.first().map_or(8, |b| usize::from(b % 21));
    let data = data.get(1..).unwrap_or(&[]);
    let mut rest = data;
    // Coalesced packets, as a receiver walks them.
    while !rest.is_empty() {
        match packet::parse(rest, cid_len) {
            Ok(h) => {
                assert!(h.pn_offset <= h.len && h.len <= rest.len());
                assert!(h.dcid.len() <= rest.len() && h.scid.len() <= rest.len());
                let _ = cid::worker_of(h.dcid);
                if h.len == 0 {
                    break;
                }
                rest = &rest[h.len..];
            }
            Err(_) => break,
        }
    }
    // Where a datagram that reached the wrong worker is passed on to:
    // only one with a short header, to the worker its ID names, never
    // back to the worker that has it.
    let me = u16::from_be_bytes([cid_len as u8, data.len() as u8]) % 5;
    if let Some(worker) = cid::owner(data, me) {
        assert!(worker != me && data[0] & 0x80 == 0);
        assert_eq!(cid::worker_of(&data[1..1 + cid::LEN]), Some(worker));
        assert_eq!(cid::owner(data, worker), None);
    }
    if let Some((v, n)) = varint::decode(data) {
        assert!(v <= varint::MAX && n <= 8);
        let mut out = [0; 8];
        let m = varint::encode(v, &mut out).unwrap();
        assert_eq!(varint::decode(&out[..m]), Some((v, m)));
    }
    // The ClientHello scan sees handshake bytes before TLS does. A message
    // judged once is judged the same with more bytes after it, and one
    // not yet whole has less than the most a message may hold.
    match hello::params(data) {
        Params::Incomplete => assert!(data.len() < hello::MAX_HELLO + 4),
        found => {
            let mut more = data.to_vec();
            more.extend_from_slice(&[0x39, 0xff, 0xa5]);
            assert_eq!(hello::params(&more), found);
        }
    }
    if data.len() >= 9 {
        let largest = u64::from_le_bytes(data[..8].try_into().unwrap()) & varint::MAX;
        let bits = [8, 16, 24, 32][usize::from(data[8] & 3)];
        let _ = packet::decode_pn(Some(largest), largest & ((1 << bits) - 1), bits);
    }
});
