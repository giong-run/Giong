//! Differential check: giong-proto against prost on the same bytes.
//!
//! For input bytes `B`:
//!
//! - If both accept `B`, prost must read giong-proto's re-encoding of its
//!   view as the same message it read from `B` (compared as prost's
//!   deterministic encoding, so NaNs compare by bits and maps in key order).
//! - If exactly one accepts, the difference must be one of the documented
//!   ones where giong-proto follows Google's protobuf and prost does not
//!   (see [`Divergence`]); anything else is a bug.

use giong_proto::{Encode, View};
use prost::Message as _;

use crate::{pb, prost_pb};

/// Known, intended differences from prost, where giong-proto does what
/// Google's C++ and Java libraries (and the conformance suite) do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Divergence {
    /// A known field arrives with a wire type it does not use. Google:
    /// an unknown field. prost: an error.
    WireTypeMismatch,
    /// A proto2 enum field holds a number the enum does not define. Google:
    /// kept as an unknown field (so it moves to the end on re-encoding).
    /// prost: stored as the field's value.
    ClosedEnumUnknown,
    /// A field tag is a varint of more than 5 bytes whose value still fits
    /// 32 bits (padded with continuation bytes). Google's C++: an error.
    /// prost: accepted.
    OverlongTag,
}

/// The result of one check.
#[derive(Debug)]
pub enum Outcome {
    /// Both accepted with the same message.
    BothAccepted,
    /// Both rejected.
    BothRejected,
    /// They differ in a documented way.
    Known(Divergence),
}

macro_rules! check_fn {
    ($name:ident, $full:expr, $view:ty, $prost:ty) => {
        /// Checks one input against prost; `Err` describes a bug.
        pub fn $name(bytes: &[u8]) -> Result<Outcome, String> {
            let p = <$prost>::decode(bytes);
            let g = <$view>::decode(bytes);
            let found = crate::schema::walk($full, bytes);
            if found.wire_mismatch {
                // prost's reading is not the reference here (it also skips
                // the wire-type check on map fields and then misreads the
                // bytes that follow). Check only that giong-proto's own
                // output reads back the same.
                if let Ok(g) = g {
                    let mut re = Vec::new();
                    g.encode_canonical(&mut re);
                    let again = <$view>::decode(&re).map_err(|e| format!("giong cannot read its own output: {e}"))?;
                    let mut re2 = Vec::new();
                    again.encode_canonical(&mut re2);
                    if re != re2 {
                        return Err(format!("giong re-encoding is not stable: {re:02x?} then {re2:02x?}"));
                    }
                }
                return Ok(Outcome::Known(Divergence::WireTypeMismatch));
            }
            match (p, g) {
                (Ok(p), Ok(g)) => {
                    let mut re = Vec::new();
                    g.encode_canonical(&mut re);
                    if re.len() != g.canonical_len() {
                        return Err(format!("canonical_len {} but wrote {} bytes", g.canonical_len(), re.len()));
                    }
                    if g.encode_to_vec().map_err(|e| e.to_string())? != bytes {
                        return Err("a view's Encode did not write its input back".into());
                    }
                    let back = <$prost>::decode(&re[..]).map_err(|e| format!("prost cannot read giong's re-encoding: {e}; re={re:02x?}"))?;
                    let want = p.encode_to_vec();
                    let got = back.encode_to_vec();
                    if want == got {
                        Ok(Outcome::BothAccepted)
                    } else if found.closed_enum_unknown {
                        Ok(Outcome::Known(Divergence::ClosedEnumUnknown))
                    } else {
                        Err(format!("values differ:\n prost: {p:?}\n giong->prost: {back:?}\n giong view: {g:?}"))
                    }
                }
                (Err(_), Err(_)) => Ok(Outcome::BothRejected),
                (Err(pe), Ok(g)) => {
                    let msg = pe.to_string();
                    Err(format!("prost rejects ({msg}) but giong accepts: {g:?}"))
                }
                (Ok(_), Err(ge)) if ge.kind() == giong_proto::ErrorKind::BadTag && has_long_varint(bytes) => {
                    Ok(Outcome::Known(Divergence::OverlongTag))
                }
                (Ok(p), Err(ge)) => Err(format!("giong rejects ({ge}) but prost accepts: {p:?}")),
            }
        }
    };
}

/// Whether `bytes` holds a varint of more than 5 bytes anywhere: 5 bytes in
/// a row with the continuation bit. Without one, no tag can be overlong.
fn has_long_varint(bytes: &[u8]) -> bool {
    bytes.windows(5).any(|w| w.iter().all(|&b| b >= 0x80))
}

check_fn!(
    check_proto3,
    ".protobuf_test_messages.proto3.TestAllTypesProto3",
    pb::protobuf_test_messages::proto3::TestAllTypesProto3View,
    prost_pb::proto3::TestAllTypesProto3
);
check_fn!(
    check_proto2,
    ".protobuf_test_messages.proto2.TestAllTypesProto2",
    pb::protobuf_test_messages::proto2::TestAllTypesProto2View,
    prost_pb::proto2::TestAllTypesProto2
);
