//! The `priority` header field and the PRIORITY_UPDATE frame's value (RFC
//! 9218): how urgent a response is, from 0 (most) to 7 (least).
//!
//! The value is a Structured Field dictionary (RFC 8941). Only the urgency
//! (`u`) is read: the incremental flag (`i`) is accepted and not used, as
//! RFC 9218 allows, since responses of equal urgency share the connection
//! in turn anyway. Members this does not understand, and an urgency out of
//! range, are ignored, so that the default stands (section 4).

/// The urgency of a request that says nothing about it (section 4.1).
pub(crate) const DEFAULT_URGENCY: u8 = 3;

/// The urgency `value` names, if it names a valid one; the last one wins
/// when it names several.
pub(crate) fn urgency(value: &[u8]) -> Option<u8> {
    let mut found = None;
    for member in value.split(|&b| b == b',') {
        // Parameters of a member (`;...`) are not used.
        let member = member.split(|&b| b == b';').next().unwrap_or_default();
        let member = member.trim_ascii();
        // `u=` and one digit; `u` alone is the boolean `u=?1`, and
        // `u=12` is out of range: neither is a valid urgency.
        if let [b'u', b'=', d @ b'0'..=b'7'] = member {
            found = Some(d - b'0');
        }
    }
    found
}

/// The urgency `value` names, or `current` when it names none: how a
/// field's value updates what is known so far.
pub(crate) fn update(current: u8, value: &[u8]) -> u8 {
    urgency(value).unwrap_or(current)
}

/// The priority of a QUIC stream that carries a response of `urgency`:
/// higher goes first, and the default urgency is QUIC's default priority,
/// 0, so that streams nobody prioritized need no call.
#[cfg_attr(not(feature = "unstable-http3"), allow(dead_code))]
pub(crate) fn quic_priority(urgency: u8) -> i32 {
    i32::from(DEFAULT_URGENCY) - i32::from(urgency)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_urgency_is_read_as_rfc_9218_defines_it() {
        let cases: &[(&str, Option<u8>)] = &[
            ("u=0", Some(0)),
            ("u=7", Some(7)),
            ("u=5, i", Some(5)),
            ("i, u=1", Some(1)),
            ("i=?0,u=2", Some(2)),
            ("u=2;x=1", Some(2)),
            ("  u=6  ", Some(6)),
            ("u=1, u=4", Some(4)),
            ("u=8", None),
            ("u=-1", None),
            ("u=1.5", None),
            ("u=12", None),
            ("u", None),
            ("u=?1", None),
            ("urgent=1", None),
            ("x=1", None),
            ("", None),
            ("i", None),
        ];
        for &(value, want) in cases {
            assert_eq!(urgency(value.as_bytes()), want, "{value:?}");
        }
        assert_eq!(update(3, b"u=1, u=9"), 1);
        assert_eq!(update(3, b"garbage"), 3);
    }

    #[test]
    fn urgencies_map_to_quic_priorities_in_order() {
        assert_eq!(quic_priority(DEFAULT_URGENCY), 0);
        for u in 0..7 {
            assert!(quic_priority(u) > quic_priority(u + 1));
        }
    }
}
