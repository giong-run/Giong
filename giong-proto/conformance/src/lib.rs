//! Generated code for the conformance messages, giong-proto's and prost's.

/// giong-proto views.
pub mod pb {
    include!(concat!(env!("OUT_DIR"), "/giong_proto.rs"));
}

/// prost structs for the same files.
pub mod prost_pb {
    pub mod conformance {
        include!(concat!(env!("OUT_DIR"), "/conformance.rs"));
    }
    pub mod proto3 {
        include!(concat!(
            env!("OUT_DIR"),
            "/protobuf_test_messages.proto3.rs"
        ));
    }
    pub mod proto2 {
        include!(concat!(
            env!("OUT_DIR"),
            "/protobuf_test_messages.proto2.rs"
        ));
    }
}

pub mod diff;
pub mod gen;
pub mod schema;
