//! Variable-length integers (RFC 9000, section 16).

/// The largest value a variable-length integer holds: 2^62 - 1.
pub const MAX: u64 = (1 << 62) - 1;

/// Reads one from the front of `buf`: the value and the bytes it took.
pub fn decode(buf: &[u8]) -> Option<(u64, usize)> {
    let first = *buf.first()?;
    let len = 1usize << (first >> 6);
    let bytes = buf.get(..len)?;
    let mut v = u64::from(first & 0x3f);
    for &b in &bytes[1..] {
        v = (v << 8) | u64::from(b);
    }
    Some((v, len))
}

/// Bytes `v` takes; `None` above [`MAX`].
pub fn size(v: u64) -> Option<usize> {
    match v {
        0..=63 => Some(1),
        64..=16_383 => Some(2),
        16_384..=1_073_741_823 => Some(4),
        1_073_741_824..=MAX => Some(8),
        _ => None,
    }
}

/// Writes `v` at the front of `out` in its shortest form, returning the
/// bytes written; `None` if it does not fit or exceeds [`MAX`].
pub fn encode(v: u64, out: &mut [u8]) -> Option<usize> {
    let len = size(v)?;
    let dst = out.get_mut(..len)?;
    let tag = (len.trailing_zeros() as u64) << (len * 8 - 2);
    dst.copy_from_slice(&(v | tag).to_be_bytes()[8 - len..]);
    Some(len)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The examples of RFC 9000, appendix A.1.
    #[test]
    fn rfc_9000_examples() {
        let cases: [(&[u8], u64); 5] = [
            (
                &[0xc2, 0x19, 0x7c, 0x5e, 0xff, 0x14, 0xe8, 0x8c],
                151_288_809_941_952_652,
            ),
            (&[0x9d, 0x7f, 0x3e, 0x7d], 494_878_333),
            (&[0x7b, 0xbd], 15_293),
            (&[0x25], 37),
            (&[0x40, 0x25], 37),
        ];
        for (bytes, v) in cases {
            assert_eq!(decode(bytes), Some((v, bytes.len())));
        }
        let mut out = [0; 8];
        for (bytes, v) in &cases[..4] {
            let n = encode(*v, &mut out).unwrap();
            assert_eq!(&out[..n], *bytes);
        }
    }

    #[test]
    fn bounds() {
        let mut out = [0; 8];
        for v in [0, 63, 64, 16_383, 16_384, 1_073_741_823, 1_073_741_824, MAX] {
            let n = encode(v, &mut out).unwrap();
            assert_eq!(decode(&out[..n]), Some((v, n)));
        }
        assert_eq!(encode(MAX + 1, &mut out), None);
        assert_eq!(encode(64, &mut out[..1]), None);
        assert_eq!(decode(&[0x40]), None);
        assert_eq!(decode(&[]), None);
    }
}
