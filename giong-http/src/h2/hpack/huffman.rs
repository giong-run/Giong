//! The Huffman code HPACK uses for string literals (RFC 7541, section 5.2).
//!
//! Encoding walks the code table. Decoding runs a state machine four bits at
//! a time: each state is an inner node of the code tree, and each step emits
//! at most one symbol (no code is shorter than five bits).

use std::sync::OnceLock;

use super::huffman_codes::CODES;

/// The symbol that only padding may contain.
const EOS: u16 = 256;

/// The length of `src` once Huffman coded, in bytes.
pub(crate) fn encoded_len(src: &[u8]) -> usize {
    let bits: usize = src.iter().map(|&b| CODES[b as usize].1 as usize).sum();
    bits.div_ceil(8)
}

/// Appends `src`, Huffman coded, to `out`.
pub(crate) fn encode(src: &[u8], out: &mut Vec<u8>) {
    out.reserve(encoded_len(src));
    let mut acc: u64 = 0;
    let mut bits: u32 = 0;
    for &b in src {
        let (code, len) = CODES[b as usize];
        acc = (acc << len) | u64::from(code);
        bits += u32::from(len);
        while bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    if bits > 0 {
        // Pad with the most significant bits of EOS: all ones.
        let pad = 8 - bits;
        out.push(((acc << pad) as u8) | ((1u8 << pad) - 1));
    }
}

/// One step of the decoder: the state after four bits, and the symbol they
/// completed, if any.
#[derive(Clone, Copy, Default)]
struct Step {
    next: u8,
    /// `EMIT`, `FAIL`.
    flags: u8,
    symbol: u8,
}

const EMIT: u8 = 1;
const FAIL: u8 = 2;

struct Machine {
    steps: Vec<[Step; 16]>,
    /// Whether input may end in this state: the bits since the last symbol
    /// are fewer than eight and all ones (a prefix of EOS).
    may_end: Vec<bool>,
}

fn machine() -> &'static Machine {
    static MACHINE: OnceLock<Machine> = OnceLock::new();
    MACHINE.get_or_init(build)
}

/// Builds the code tree, then the state machine over its inner nodes.
fn build() -> Machine {
    // Nodes: children of inner nodes (`u16::MAX` = none), symbols of leaves.
    const NONE: u16 = u16::MAX;
    let mut children: Vec<[u16; 2]> = vec![[NONE; 2]];
    let mut leaf: Vec<Option<u16>> = vec![None];
    for (symbol, &(code, len)) in CODES.iter().enumerate() {
        let mut node = 0usize;
        for i in (0..len).rev() {
            let bit = ((code >> i) & 1) as usize;
            if children[node][bit] == NONE {
                children.push([NONE; 2]);
                leaf.push(None);
                children[node][bit] = (children.len() - 1) as u16;
            }
            node = children[node][bit] as usize;
        }
        leaf[node] = Some(symbol as u16);
    }
    // Number the inner nodes: they are the states.
    let mut state_of = vec![u8::MAX; children.len()];
    let mut inner = Vec::new();
    for node in 0..children.len() {
        if leaf[node].is_none() {
            state_of[node] = inner.len() as u8;
            inner.push(node);
        }
    }
    assert_eq!(inner.len(), 256, "HPACK's code has 257 symbols");
    // Inner nodes reached from the root by fewer than eight one bits.
    let mut may_end = vec![false; 256];
    let mut node = 0usize;
    for _ in 0..8 {
        may_end[state_of[node] as usize] = true;
        node = children[node][1] as usize;
        if leaf[node].is_some() {
            break;
        }
    }

    let mut steps = vec![[Step::default(); 16]; 256];
    for (state, &start) in inner.iter().enumerate() {
        for nibble in 0..16u8 {
            let mut node = start;
            let mut step = Step::default();
            for i in (0..4).rev() {
                let bit = ((nibble >> i) & 1) as usize;
                node = children[node][bit] as usize;
                if let Some(symbol) = leaf[node] {
                    if symbol == EOS {
                        step.flags |= FAIL;
                        break;
                    }
                    step.flags |= EMIT;
                    step.symbol = symbol as u8;
                    node = 0;
                }
            }
            step.next = state_of[node];
            steps[state][nibble as usize] = step;
        }
    }
    Machine { steps, may_end }
}

/// Appends the Huffman-decoded `src` to `out`. `Err` for EOS in the data,
/// padding longer than seven bits, or padding that is not all ones.
pub(crate) fn decode(src: &[u8], out: &mut Vec<u8>) -> Result<(), ()> {
    let machine = machine();
    out.reserve(src.len() * 8 / 5);
    let mut state = 0u8;
    for &b in src {
        for nibble in [b >> 4, b & 0x0f] {
            let step = machine.steps[state as usize][nibble as usize];
            if step.flags & FAIL != 0 {
                return Err(());
            }
            if step.flags & EMIT != 0 {
                out.push(step.symbol);
            }
            state = step.next;
        }
    }
    if machine.may_end[state as usize] {
        Ok(())
    } else {
        Err(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(s: &str) -> Vec<u8> {
        let s: String = s.split_whitespace().collect();
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
            .collect()
    }

    #[test]
    fn matches_the_examples_of_rfc_7541() {
        let cases = [
            ("www.example.com", "f1e3 c2e5 f23a 6ba0 ab90 f4ff"),
            ("no-cache", "a8eb 1064 9cbf"),
            ("custom-key", "25a8 49e9 5ba9 7d7f"),
            ("custom-value", "25a8 49e9 5bb8 e8b4 bf"),
            ("302", "6402"),
            ("private", "aec3 771a 4b"),
            (
                "Mon, 21 Oct 2013 20:13:21 GMT",
                "d07a be94 1054 d444 a820 0595 040b 8166 e082 a62d 1bff",
            ),
            (
                "https://www.example.com",
                "9d29 ad17 1863 c78f 0b97 c8e9 ae82 ae43 d3",
            ),
        ];
        for (plain, coded) in cases {
            let coded = hex(coded);
            let mut out = Vec::new();
            encode(plain.as_bytes(), &mut out);
            assert_eq!(out, coded, "{plain}");
            assert_eq!(encoded_len(plain.as_bytes()), coded.len());
            let mut back = Vec::new();
            decode(&coded, &mut back).unwrap();
            assert_eq!(back, plain.as_bytes());
        }
    }

    #[test]
    fn every_byte_round_trips() {
        let all: Vec<u8> = (0..=255).collect();
        let mut coded = Vec::new();
        encode(&all, &mut coded);
        let mut back = Vec::new();
        decode(&coded, &mut back).unwrap();
        assert_eq!(back, all);
        for b in 0..=255u8 {
            let mut coded = Vec::new();
            encode(&[b, b, b], &mut coded);
            let mut back = Vec::new();
            decode(&coded, &mut back).unwrap();
            assert_eq!(back, [b, b, b]);
        }
    }

    #[test]
    fn refuses_bad_padding_and_eos() {
        // "a" is 00011 (5 bits); padding of three zero bits is not EOS.
        assert!(decode(&[0b0001_1000], &mut Vec::new()).is_err());
        assert!(decode(&[0b0001_1111], &mut Vec::new()).is_ok());
        // Eight bits of padding: a whole byte of ones after "a".
        assert!(decode(&[0b0001_1111, 0xff], &mut Vec::new()).is_err());
        // EOS itself (30 ones), padded with two more.
        assert!(decode(&[0xff, 0xff, 0xff, 0xff], &mut Vec::new()).is_err());
    }
}
