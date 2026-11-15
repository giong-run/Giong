//! The native engine's packet parsing, which every datagram from the
//! network reaches before any authentication: never panic, and whatever a
//! header claims stays inside the datagram.
#![no_main]

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
    if let Some((v, n)) = varint::decode(data) {
        assert!(v <= varint::MAX && n <= 8);
        let mut out = [0; 8];
        let m = varint::encode(v, &mut out).unwrap();
        assert_eq!(varint::decode(&out[..m]), Some((v, m)));
    }
    if data.len() >= 9 {
        let largest = u64::from_le_bytes(data[..8].try_into().unwrap()) & varint::MAX;
        let bits = [8, 16, 24, 32][usize::from(data[8] & 3)];
        let _ = packet::decode_pn(Some(largest), largest & ((1 << bits) - 1), bits);
    }
});
