//! A quick stand-in for the fuzz target in fuzz/ (which needs nightly):
//! millions of random and mutated packets through the native parser.

use giong_quic::native::{cid, packet};

fn xorshift(s: &mut u64) -> u64 {
    *s ^= *s << 13;
    *s ^= *s >> 7;
    *s ^= *s << 17;
    *s
}

#[test]
fn random_and_mutated_packets_never_panic() {
    // A valid version 1 Initial header to mutate, and random bytes.
    let mut seed_packet = vec![
        0xc3, 0, 0, 0, 1, 8, 1, 2, 3, 4, 5, 6, 7, 8, 0, 0, 0x41, 0x00,
    ];
    seed_packet.resize(1200, 0xaa);
    let mut s = 0x9e37_79b9_7f4a_7c15u64;
    for i in 0..2_000_000u32 {
        let mut p = if i % 2 == 0 {
            seed_packet.clone()
        } else {
            let n = (xorshift(&mut s) % 64) as usize;
            (0..n).map(|_| xorshift(&mut s) as u8).collect()
        };
        for _ in 0..(xorshift(&mut s) % 4) {
            if !p.is_empty() {
                let at = (xorshift(&mut s) as usize) % p.len();
                p[at] = xorshift(&mut s) as u8;
            }
        }
        if xorshift(&mut s) % 8 == 0 {
            let cut = (xorshift(&mut s) as usize) % (p.len() + 1);
            p.truncate(cut);
        }
        // Passed on only to another worker, the one its ID names.
        let me = (xorshift(&mut s) % 4) as u16;
        if let Some(worker) = cid::owner(&p, me) {
            assert!(worker != me && p[0] & 0x80 == 0);
            assert_eq!(cid::worker_of(&p[1..1 + cid::LEN]), Some(worker));
        }
        let cid_len = (xorshift(&mut s) % 21) as usize;
        if let Ok(h) = packet::parse(&p, cid_len) {
            assert!(h.pn_offset <= h.len && h.len <= p.len(), "{h:?}");
        }
    }
}
