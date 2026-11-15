//! QUIC packets: headers (RFC 9000, section 17; RFC 9369 for version 2),
//! packet numbers, and protection applied in place in the buffer that goes
//! to or came from the socket (RFC 9001, section 5).
//!
//! Nothing here allocates or copies payload: a received packet is opened
//! inside the receive buffer, and a packet to send is written into the send
//! buffer (a GSO batch) and sealed there.

use rustls::quic::{DirectionalKeys, HeaderProtectionKey, PacketKey};

use super::varint;

/// QUIC version 1 (RFC 9000).
pub const V1: u32 = 1;
/// QUIC version 2 (RFC 9369).
pub const V2: u32 = 0x6b33_43cf;
/// The longest connection ID QUIC version 1 and 2 allow.
pub const MAX_CID: usize = 20;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Initial,
    ZeroRtt,
    Handshake,
    Retry,
    /// 1-RTT.
    Short,
    VersionNegotiation,
    /// A long header of a version this endpoint does not speak: answered
    /// with version negotiation.
    UnknownVersion,
}

/// A parsed header. Fields after the packet number offset are still
/// protected.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Header<'a> {
    pub kind: Kind,
    /// 0 for a short header.
    pub version: u32,
    pub dcid: &'a [u8],
    /// Empty for a short header.
    pub scid: &'a [u8],
    /// Initial: the address validation token. Retry: the retry token.
    pub token: &'a [u8],
    /// Where the packet number starts (for Retry, version negotiation and
    /// unknown versions: where the header ends).
    pub pn_offset: usize,
    /// Bytes of this packet in the datagram; coalesced packets follow.
    pub len: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    /// Ends before the header does.
    Truncated,
    /// The fixed bit (0x40) is clear: not a QUIC v1/v2 packet.
    FixedBit,
    /// A connection ID longer than 20 bytes in a version 1 or 2 packet.
    CidTooLong,
    /// The Length field runs past the datagram, or is too short for a
    /// packet number and sample.
    Length,
    /// The sample for header protection is not there.
    NoSample,
    /// Header protection or decryption failed: not for these keys, or
    /// tampered with.
    Crypto,
}

fn long_kind(version: u32, bits: u8) -> Kind {
    match (version, bits) {
        (V2, 1) | (V1, 0) => Kind::Initial,
        (V2, 2) | (V1, 1) => Kind::ZeroRtt,
        (V2, 3) | (V1, 2) => Kind::Handshake,
        _ => Kind::Retry,
    }
}

fn long_bits(version: u32, kind: Kind) -> u8 {
    let v1 = match kind {
        Kind::Initial => 0,
        Kind::ZeroRtt => 1,
        Kind::Handshake => 2,
        _ => 3,
    };
    if version == V2 {
        (v1 + 1) & 3
    } else {
        v1
    }
}

struct Reader<'a> {
    buf: &'a [u8],
    at: usize,
}

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8], Error> {
        let s = self
            .buf
            .get(self.at..self.at.checked_add(n).ok_or(Error::Truncated)?)
            .ok_or(Error::Truncated)?;
        self.at += n;
        Ok(s)
    }
    fn u8(&mut self) -> Result<u8, Error> {
        Ok(self.take(1)?[0])
    }
    fn varint(&mut self) -> Result<u64, Error> {
        let (v, n) =
            varint::decode(&self.buf[self.at.min(self.buf.len())..]).ok_or(Error::Truncated)?;
        self.at += n;
        Ok(v)
    }
}

/// Parses the header of the first packet in `datagram`. `local_cid_len` is
/// the length of the connection IDs this endpoint issues (short headers do
/// not carry it).
pub fn parse(datagram: &[u8], local_cid_len: usize) -> Result<Header<'_>, Error> {
    let mut r = Reader {
        buf: datagram,
        at: 0,
    };
    let first = r.u8()?;
    if first & 0x80 == 0 {
        if first & 0x40 == 0 {
            return Err(Error::FixedBit);
        }
        let dcid = r.take(local_cid_len)?;
        return Ok(Header {
            kind: Kind::Short,
            version: 0,
            dcid,
            scid: &[],
            token: &[],
            pn_offset: r.at,
            len: datagram.len(),
        });
    }
    let version = u32::from_be_bytes(r.take(4)?.try_into().expect("four bytes"));
    let dcid_len = usize::from(r.u8()?);
    let dcid = r.take(dcid_len)?;
    let scid_len = usize::from(r.u8()?);
    let scid = r.take(scid_len)?;
    let mut h = Header {
        kind: Kind::UnknownVersion,
        version,
        dcid,
        scid,
        token: &[],
        pn_offset: r.at,
        len: datagram.len(),
    };
    if version == 0 {
        h.kind = Kind::VersionNegotiation;
        return Ok(h);
    }
    if version != V1 && version != V2 {
        return Ok(h);
    }
    if first & 0x40 == 0 {
        return Err(Error::FixedBit);
    }
    if dcid_len > MAX_CID || scid_len > MAX_CID {
        return Err(Error::CidTooLong);
    }
    h.kind = long_kind(version, (first >> 4) & 3);
    if h.kind == Kind::Retry {
        // Retry token, then a 16-byte integrity tag.
        let rest = datagram.len() - r.at;
        h.token = r.take(rest.checked_sub(16).ok_or(Error::Truncated)?)?;
        h.pn_offset = r.at;
        return Ok(h);
    }
    if h.kind == Kind::Initial {
        let n = usize::try_from(r.varint()?).map_err(|_| Error::Length)?;
        h.token = r.take(n)?;
    }
    let length = usize::try_from(r.varint()?).map_err(|_| Error::Length)?;
    h.pn_offset = r.at;
    // A packet number (1-4 bytes) and a 16-byte sample must fit.
    if length < 4 + 16 - 4 || length > datagram.len() - r.at {
        return Err(Error::Length);
    }
    h.len = r.at + length;
    Ok(h)
}

/// The full packet number from its truncated form (RFC 9000, appendix A.3).
pub fn decode_pn(largest: Option<u64>, truncated: u64, bits: u32) -> u64 {
    let expected = largest.map_or(0, |l| l + 1);
    let win = 1u64 << bits;
    let hwin = win / 2;
    let mask = win - 1;
    let candidate = (expected & !mask) | truncated;
    if candidate + hwin <= expected && candidate < (1 << 62) - win {
        candidate + win
    } else if candidate > expected + hwin && candidate >= win {
        candidate - win
    } else {
        candidate
    }
}

/// Bytes needed to send packet number `pn` when the peer has acknowledged
/// up to `largest_acked` (RFC 9000, appendix A.2).
pub fn pn_len(pn: u64, largest_acked: Option<u64>) -> usize {
    let unacked = match largest_acked {
        Some(l) => pn - l,
        None => pn + 1,
    };
    let bits = 64 - unacked.leading_zeros() + 1;
    (bits.div_ceil(8) as usize).clamp(1, 4)
}

/// Removes header protection and decrypts the packet `h` describes, in
/// place in `datagram`. Returns the packet number and the range of the
/// plaintext payload within `datagram`.
pub fn open(
    datagram: &mut [u8],
    h: &Header<'_>,
    hp: &dyn HeaderProtectionKey,
    key: &dyn PacketKey,
    largest: Option<u64>,
) -> Result<(u64, std::ops::Range<usize>), Error> {
    let packet = datagram.get_mut(..h.len).ok_or(Error::Length)?;
    let pn_at = h.pn_offset;
    let sample_at = pn_at + 4;
    let sample_len = hp.sample_len();
    if packet.len() < sample_at + sample_len {
        return Err(Error::NoSample);
    }
    let (head, rest) = packet.split_at_mut(sample_at);
    let sample = &rest[..sample_len];
    let (first, after_first) = head.split_at_mut(1);
    hp.decrypt_in_place(sample, &mut first[0], &mut after_first[pn_at - 1..])
        .map_err(|_| Error::Crypto)?;
    let len = usize::from(packet[0] & 3) + 1;
    let truncated = packet[pn_at..pn_at + len]
        .iter()
        .fold(0u64, |a, &b| (a << 8) | u64::from(b));
    let pn = decode_pn(largest, truncated, 8 * len as u32);
    let (header, payload) = packet.split_at_mut(pn_at + len);
    let plain = key
        .decrypt_in_place(pn, header, payload)
        .map_err(|_| Error::Crypto)?
        .len();
    Ok((pn, pn_at + len..pn_at + len + plain))
}

/// Writes a long header for `kind` into `out` with room for a payload of
/// `payload_len` bytes (the frames, not counting the tag) and returns
/// `(header_len, pn_offset)`. The packet number is written in `pn_len`
/// bytes; the reserved bits are zero.
#[allow(clippy::too_many_arguments)]
pub fn write_long(
    out: &mut [u8],
    kind: Kind,
    version: u32,
    dcid: &[u8],
    scid: &[u8],
    token: &[u8],
    pn: u64,
    pn_len: usize,
    payload_len: usize,
    tag_len: usize,
) -> Option<(usize, usize)> {
    let mut at = 0;
    let mut put = |b: &[u8]| -> Option<()> {
        out.get_mut(at..at + b.len())?.copy_from_slice(b);
        at += b.len();
        Some(())
    };
    let first = 0xc0 | (long_bits(version, kind) << 4) | (pn_len as u8 - 1);
    put(&[first])?;
    put(&version.to_be_bytes())?;
    put(&[dcid.len() as u8])?;
    put(dcid)?;
    put(&[scid.len() as u8])?;
    put(scid)?;
    let mut tmp = [0u8; 8];
    if kind == Kind::Initial {
        let n = varint::encode(token.len() as u64, &mut tmp)?;
        put(&tmp[..n])?;
        put(token)?;
    }
    // Length covers the packet number, payload and tag. Always 2 bytes so
    // the header size does not depend on the payload (datagrams up to
    // 16383 bytes).
    let length = (pn_len + payload_len + tag_len) as u64;
    if length > 16_383 {
        return None;
    }
    put(&(0x4000 | length as u16).to_be_bytes())?;
    put(&pn.to_be_bytes()[8 - pn_len..])?;
    let end = at;
    Some((end, end - pn_len))
}

/// Encrypts the payload and adds header protection, in place. `packet`
/// holds the header (ending with the packet number at `pn_offset`), the
/// plaintext payload, and room for the tag at its end.
pub fn seal(
    packet: &mut [u8],
    pn_offset: usize,
    pn: u64,
    keys: &DirectionalKeys,
) -> Result<(), Error> {
    let tag_len = keys.packet.tag_len();
    let pn_len = usize::from(packet[0] & 3) + 1;
    let header_len = pn_offset + pn_len;
    let end = packet
        .len()
        .checked_sub(tag_len)
        .filter(|&e| e >= header_len)
        .ok_or(Error::Length)?;
    let (header, rest) = packet.split_at_mut(header_len);
    let tag = keys
        .packet
        .encrypt_in_place(pn, header, &mut rest[..end - header_len])
        .map_err(|_| Error::Crypto)?;
    packet[end..].copy_from_slice(tag.as_ref());
    let sample_at = pn_offset + 4;
    let sample_len = keys.header.sample_len();
    if packet.len() < sample_at + sample_len {
        return Err(Error::NoSample);
    }
    let (head, rest) = packet.split_at_mut(sample_at);
    let (first, after_first) = head.split_at_mut(1);
    keys.header
        .encrypt_in_place(
            &rest[..sample_len],
            &mut first[0],
            &mut after_first[pn_offset - 1..pn_offset - 1 + pn_len],
        )
        .map_err(|_| Error::Crypto)
}

#[cfg(test)]
mod tests {
    use rustls::quic::{Keys, Version};
    use rustls::Side;

    use super::*;

    fn initial_keys(version: Version, side: Side) -> Keys {
        let suite = rustls::crypto::ring::default_provider()
            .cipher_suites
            .iter()
            .find_map(|s| match s {
                rustls::SupportedCipherSuite::Tls13(t)
                    if t.common.suite == rustls::CipherSuite::TLS13_AES_128_GCM_SHA256 =>
                {
                    Some(*t)
                }
                _ => None,
            })
            .unwrap();
        Keys::initial(
            version,
            suite,
            suite.quic.unwrap(),
            &[0x83, 0x94, 0xc8, 0xf0, 0x3e, 0x51, 0x57, 0x08],
            side,
        )
    }

    /// The server's Initial packet of RFC 9369, appendix A.3 (QUIC
    /// version 2), as rustls tests it: the header written by `write_long`
    /// and sealed in place by `seal` comes out byte for byte, and `parse`
    /// and `open` give the ServerHello back.
    #[test]
    fn rfc_9369_server_initial() {
        let payload: [u8; 99] = [
            0x02, 0x00, 0x00, 0x00, 0x00, 0x06, 0x00, 0x40, 0x5a, 0x02, 0x00, 0x00, 0x56, 0x03,
            0x03, 0xee, 0xfc, 0xe7, 0xf7, 0xb3, 0x7b, 0xa1, 0xd1, 0x63, 0x2e, 0x96, 0x67, 0x78,
            0x25, 0xdd, 0xf7, 0x39, 0x88, 0xcf, 0xc7, 0x98, 0x25, 0xdf, 0x56, 0x6d, 0xc5, 0x43,
            0x0b, 0x9a, 0x04, 0x5a, 0x12, 0x00, 0x13, 0x01, 0x00, 0x00, 0x2e, 0x00, 0x33, 0x00,
            0x24, 0x00, 0x1d, 0x00, 0x20, 0x9d, 0x3c, 0x94, 0x0d, 0x89, 0x69, 0x0b, 0x84, 0xd0,
            0x8a, 0x60, 0x99, 0x3c, 0x14, 0x4e, 0xca, 0x68, 0x4d, 0x10, 0x81, 0x28, 0x7c, 0x83,
            0x4d, 0x53, 0x11, 0xbc, 0xf3, 0x2b, 0xb9, 0xda, 0x1a, 0x00, 0x2b, 0x00, 0x02, 0x03,
            0x04,
        ];
        let expected: [u8; 135] = [
            0xdc, 0x6b, 0x33, 0x43, 0xcf, 0x00, 0x08, 0xf0, 0x67, 0xa5, 0x50, 0x2a, 0x42, 0x62,
            0xb5, 0x00, 0x40, 0x75, 0xd9, 0x2f, 0xaa, 0xf1, 0x6f, 0x05, 0xd8, 0xa4, 0x39, 0x8c,
            0x47, 0x08, 0x96, 0x98, 0xba, 0xee, 0xa2, 0x6b, 0x91, 0xeb, 0x76, 0x1d, 0x9b, 0x89,
            0x23, 0x7b, 0xbf, 0x87, 0x26, 0x30, 0x17, 0x91, 0x53, 0x58, 0x23, 0x00, 0x35, 0xf7,
            0xfd, 0x39, 0x45, 0xd8, 0x89, 0x65, 0xcf, 0x17, 0xf9, 0xaf, 0x6e, 0x16, 0x88, 0x6c,
            0x61, 0xbf, 0xc7, 0x03, 0x10, 0x6f, 0xba, 0xf3, 0xcb, 0x4c, 0xfa, 0x52, 0x38, 0x2d,
            0xd1, 0x6a, 0x39, 0x3e, 0x42, 0x75, 0x75, 0x07, 0x69, 0x80, 0x75, 0xb2, 0xc9, 0x84,
            0xc7, 0x07, 0xf0, 0xa0, 0x81, 0x2d, 0x8c, 0xd5, 0xa6, 0x88, 0x1e, 0xaf, 0x21, 0xce,
            0xda, 0x98, 0xf4, 0xbd, 0x23, 0xf6, 0xfe, 0x1a, 0x3e, 0x2c, 0x43, 0xed, 0xd9, 0xce,
            0x7c, 0xa8, 0x4b, 0xed, 0x85, 0x21, 0xe2, 0xe1, 0x40,
        ];
        let scid = [0xf0, 0x67, 0xa5, 0x50, 0x2a, 0x42, 0x62, 0xb5];
        let server = initial_keys(Version::V2, Side::Server);
        let mut buf = [0u8; 135];
        let (hlen, pn_offset) = write_long(
            &mut buf,
            Kind::Initial,
            V2,
            &[],
            &scid,
            &[],
            1,
            2,
            payload.len(),
            16,
        )
        .unwrap();
        assert_eq!(hlen, 20);
        buf[hlen..hlen + payload.len()].copy_from_slice(&payload);
        seal(&mut buf, pn_offset, 1, &server.local).unwrap();
        assert_eq!(buf, expected);

        let client = initial_keys(Version::V2, Side::Client);
        let mut rx = expected;
        let h = parse(&rx, 8).unwrap();
        assert_eq!(
            (h.kind, h.version, h.dcid, h.scid, h.len),
            (Kind::Initial, V2, &[][..], &scid[..], 135)
        );
        let h = Header {
            dcid: &[],
            scid: &[],
            token: &[],
            ..h
        };
        let (pn, range) = open(
            &mut rx,
            &h,
            &*client.remote.header,
            &*client.remote.packet,
            None,
        )
        .unwrap();
        assert_eq!(pn, 1);
        assert_eq!(&rx[range], &payload[..]);

        // One flipped bit anywhere fails authentication.
        let mut bad = expected;
        bad[60] ^= 1;
        let h2 = parse(&bad, 8).unwrap();
        let h2 = Header {
            dcid: &[],
            scid: &[],
            token: &[],
            ..h2
        };
        assert_eq!(
            open(
                &mut bad,
                &h2,
                &*client.remote.header,
                &*client.remote.packet,
                None
            ),
            Err(Error::Crypto)
        );
    }

    /// RFC 9000, appendix A.3.
    #[test]
    fn packet_number_decoding() {
        assert_eq!(decode_pn(Some(0xa82f30ea), 0x9b32, 16), 0xa82f9b32);
        assert_eq!(decode_pn(None, 0, 8), 0);
        assert_eq!(decode_pn(Some(255), 0, 8), 256);
        assert_eq!(decode_pn(Some(0xabe8b3), 0xac5c02 & 0xffff, 16), 0xac5c02);
    }

    /// RFC 9000, appendix A.2.
    #[test]
    fn packet_number_length() {
        assert_eq!(pn_len(0xac5c02, Some(0xabe8b3)), 2);
        assert_eq!(pn_len(0xace8fe, Some(0xabe8b3)), 3);
        assert_eq!(pn_len(0, None), 1);
    }

    #[test]
    fn malformed_headers_are_refused() {
        assert_eq!(parse(&[], 8), Err(Error::Truncated));
        assert_eq!(parse(&[0x00, 1, 2], 0), Err(Error::FixedBit));
        assert_eq!(parse(&[0x40, 1, 2], 8), Err(Error::Truncated));
        // Initial with a 21-byte destination ID.
        let mut p = vec![0xc0, 0, 0, 0, 1, 21];
        p.extend([0; 21]);
        p.extend([0, 0, 0x40, 0x20]);
        p.resize(100, 0);
        assert_eq!(parse(&p, 8), Err(Error::CidTooLong));
        // Length past the end.
        let p = [0xc0, 0, 0, 0, 1, 0, 0, 0, 0x44, 0x00, 0, 0, 0, 0];
        assert_eq!(parse(&p, 8), Err(Error::Length));
        // Version negotiation and an unknown version parse without a length.
        let vn = [0x80, 0, 0, 0, 0, 1, 7, 1, 9, 0, 0, 0, 1];
        assert_eq!(parse(&vn, 8).unwrap().kind, Kind::VersionNegotiation);
        let unk = [0xc0, 0x0a, 0x0a, 0x0a, 0x0a, 0, 0];
        assert_eq!(parse(&unk, 8).unwrap().kind, Kind::UnknownVersion);
    }
}
