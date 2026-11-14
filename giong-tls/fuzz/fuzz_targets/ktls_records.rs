//! What kernel TLS reads of the peer's records outside the kernel: the record
//! framing it follows during the handshake, and the handshake messages that
//! arrive after it (tickets, key updates). Never panics, and gives the same
//! answer however the bytes are split into reads or records.
#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    // Byte 0: seed for the piece sizes. The rest: the peer's bytes, read
    // both as a record stream and as handshake messages.
    let [seed, input @ ..] = data else {
        return;
    };
    let mut state = u32::from(*seed) | 1;
    let mut piece = || {
        state ^= state << 13;
        state ^= state >> 17;
        state ^= state << 5;
        state as usize
    };
    giong_tls::fuzzing::check_record_framing(input, &mut piece);
    giong_tls::fuzzing::check_handshake_messages(input, &mut piece);
});
