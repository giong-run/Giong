//! The one thing QUIC has to read in a ClientHello itself: under which
//! codepoint the client sent its transport parameters.
//!
//! QUIC version 1 carries them in the `quic_transport_parameters`
//! extension, 0x39; the drafts used 0xffa5. A version 1 server must refuse
//! a ClientHello without the first with the `missing_extension` alert
//! (RFC 9001, section 8.2), and rustls reads either one whatever the
//! version, so the server looks before rustls does.

/// `quic_transport_parameters` (RFC 9001, section 8.2).
pub const FINAL: u16 = 0x39;
/// The codepoint the drafts of QUIC used for the same extension.
pub const DRAFT: u16 = 0xffa5;

/// The largest handshake message rustls takes; a ClientHello that claims
/// more is refused there, whatever it holds.
pub const MAX_HELLO: usize = 0xffff;

/// What [`params`] found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Params {
    /// The message has not arrived whole.
    Incomplete,
    /// The extension of version 1 is there.
    Final,
    /// Only the extension of the drafts is there.
    DraftOnly,
    /// Neither is there.
    Neither,
    /// These bytes are no ClientHello that TLS will take: another message,
    /// one too large, or one whose lengths do not add up.
    Malformed,
}

/// Looks for the transport parameters in a ClientHello. `stream` is the
/// client's handshake data from its first byte: the four bytes of the
/// handshake header and as much of the message as has arrived.
pub fn params(stream: &[u8]) -> Params {
    let Some(header) = stream.get(..4) else {
        return Params::Incomplete;
    };
    // client_hello(1).
    if header[0] != 1 {
        return Params::Malformed;
    }
    let len = usize::from(header[1]) << 16 | usize::from(header[2]) << 8 | usize::from(header[3]);
    if len > MAX_HELLO {
        return Params::Malformed;
    }
    let Some(body) = stream.get(4..4 + len) else {
        return Params::Incomplete;
    };
    extensions(body).map_or(Params::Malformed, |extensions| {
        let mut found = Params::Neither;
        let mut rest = extensions;
        while !rest.is_empty() {
            let Some((kind, after)) = take_u16(rest) else {
                return Params::Malformed;
            };
            let Some((_, after)) = take_vec(after, 2) else {
                return Params::Malformed;
            };
            rest = after;
            match kind {
                FINAL => return Params::Final,
                DRAFT => found = Params::DraftOnly,
                _ => {}
            }
        }
        found
    })
}

/// The extensions of a ClientHello body (RFC 8446, section 4.1.2); empty
/// when it ends before them.
fn extensions(body: &[u8]) -> Option<&[u8]> {
    // legacy_version and random.
    let rest = body.get(34..)?;
    let (_session_id, rest) = take_vec(rest, 1)?;
    let (_cipher_suites, rest) = take_vec(rest, 2)?;
    let (_compression, rest) = take_vec(rest, 1)?;
    if rest.is_empty() {
        return Some(rest);
    }
    let (extensions, rest) = take_vec(rest, 2)?;
    rest.is_empty().then_some(extensions)
}

fn take_u16(buf: &[u8]) -> Option<(u16, &[u8])> {
    let bytes = buf.get(..2)?;
    Some((u16::from_be_bytes([bytes[0], bytes[1]]), &buf[2..]))
}

/// A vector with a length prefix of `prefix` bytes (1 or 2), and what
/// follows it.
fn take_vec(buf: &[u8], prefix: usize) -> Option<(&[u8], &[u8])> {
    let head = buf.get(..prefix)?;
    let len = head.iter().fold(0usize, |n, &b| n << 8 | usize::from(b));
    let rest = &buf[prefix..];
    (rest.len() >= len).then(|| rest.split_at(len))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A ClientHello with these extensions, each with a two-byte body.
    fn hello(extensions: &[u16]) -> Vec<u8> {
        let mut body = vec![3, 3];
        body.extend_from_slice(&[7; 32]);
        body.extend_from_slice(&[2, 9, 9]); // session id
        body.extend_from_slice(&[0, 2, 0x13, 0x01]); // cipher suites
        body.extend_from_slice(&[1, 0]); // compression
        let mut list = Vec::new();
        for kind in extensions {
            list.extend_from_slice(&kind.to_be_bytes());
            list.extend_from_slice(&[0, 2, 0xaa, 0xbb]);
        }
        body.extend_from_slice(&(list.len() as u16).to_be_bytes());
        body.extend_from_slice(&list);
        let mut out = vec![1, 0];
        out.extend_from_slice(&(body.len() as u16).to_be_bytes());
        out.extend_from_slice(&body);
        out
    }

    #[test]
    fn the_codepoint_of_the_parameters_is_found() {
        assert_eq!(params(&hello(&[0, 0x2b, FINAL, 0x33])), Params::Final);
        assert_eq!(params(&hello(&[0, DRAFT, 0x33])), Params::DraftOnly);
        // Both: version 1's is there, which is all that is asked.
        assert_eq!(params(&hello(&[DRAFT, FINAL])), Params::Final);
        assert_eq!(params(&hello(&[FINAL, DRAFT])), Params::Final);
        assert_eq!(params(&hello(&[0, 0x2b])), Params::Neither);
        assert_eq!(params(&hello(&[])), Params::Neither);
        // What follows the message is not looked at.
        let mut more = hello(&[DRAFT]);
        more.extend_from_slice(&[0xff; 9]);
        assert_eq!(params(&more), Params::DraftOnly);
    }

    #[test]
    fn a_message_is_judged_only_when_it_is_whole() {
        let whole = hello(&[0, DRAFT, 0x33]);
        for cut in 0..whole.len() {
            assert_eq!(params(&whole[..cut]), Params::Incomplete, "{cut}");
        }
    }

    #[test]
    fn what_tls_will_refuse_is_left_to_tls() {
        // Another handshake message.
        let mut other = hello(&[DRAFT]);
        other[0] = 2;
        assert_eq!(params(&other), Params::Malformed);
        // More than rustls takes.
        assert_eq!(params(&[1, 1, 0, 0]), Params::Malformed);
        // Lengths inside that reach past the message, at every place.
        let whole = hello(&[0, DRAFT]);
        for at in [38, 41, 45, 47, 51, 57] {
            let mut bad = whole.clone();
            bad[at] = 0xff;
            assert_eq!(params(&bad), Params::Malformed, "{at}");
        }
        // Bytes after the extensions.
        let mut trailing = whole.clone();
        trailing.push(0);
        trailing[3] += 1;
        assert_eq!(params(&trailing), Params::Malformed);
        // A body that ends inside the fixed part.
        assert_eq!(params(&[1, 0, 0, 3, 3, 3, 0]), Params::Malformed);
    }
}
