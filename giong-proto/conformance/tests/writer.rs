//! Message structs (the encoding side) against prost: the same message
//! built both ways must encode to bytes prost reads as equal, and the
//! giong-proto view of those bytes must read the same values.

use giong_proto::{Encode, Enum, View};
use giong_proto_conformance::pb::google::protobuf as wkt;
use giong_proto_conformance::pb::protobuf_test_messages::proto2 as g2;
use giong_proto_conformance::pb::protobuf_test_messages::proto3 as g3;
use giong_proto_conformance::prost_pb::{proto2 as p2, proto3 as p3};
use prost::Message as _;

#[test]
fn proto3_all_kinds_of_fields() {
    let nested = g3::test_all_types_proto3::NestedMessage {
        a: 7,
        ..Default::default()
    };
    let inner = g3::TestAllTypesProto3 {
        optional_int32: -5,
        optional_string: "inner",
        ..Default::default()
    };
    let nested_list = [
        g3::test_all_types_proto3::NestedMessage {
            a: 1,
            ..Default::default()
        },
        g3::test_all_types_proto3::NestedMessage {
            a: -2,
            ..Default::default()
        },
    ];
    let map_msg = [(
        "k1",
        g3::test_all_types_proto3::NestedMessage {
            a: 3,
            ..Default::default()
        },
    )];
    let wrapper = wkt::Int32Value {
        value: 0,
        ..Default::default()
    };
    let w = g3::TestAllTypesProto3 {
        optional_int32: -1,
        optional_int64: i64::MIN,
        optional_uint32: u32::MAX,
        optional_uint64: 1 << 40,
        optional_sint32: -100,
        optional_sint64: i64::MAX,
        optional_fixed32: 9,
        optional_fixed64: 10,
        optional_sfixed32: -11,
        optional_sfixed64: -12,
        optional_float: -0.0,
        optional_double: f64::INFINITY,
        optional_bool: true,
        optional_string: "Gióng",
        optional_bytes: &[0, 255, 7],
        optional_nested_message: Some(&nested),
        optional_nested_enum: Enum::from(g3::test_all_types_proto3::NestedEnum::Baz),
        optional_foreign_enum: Enum::from_raw(42), // a value the enum does not define
        recursive_message: Some(&inner),
        repeated_int32: &[1, -1, 300],
        repeated_string: &["a", "", "ccc"],
        repeated_nested_message: &nested_list,
        packed_sint64: &[-3, 0, 3],
        unpacked_fixed32: &[1, 2],
        map_string_nested_message: &map_msg,
        map_int32_int32: &[(1, 2), (3, 4)],
        optional_int32_wrapper: Some(&wrapper),
        oneof_field: Some(g3::test_all_types_proto3::OneofField::OneofString("chosen")),
        ..Default::default()
    };
    let p = p3::TestAllTypesProto3 {
        optional_int32: -1,
        optional_int64: i64::MIN,
        optional_uint32: u32::MAX,
        optional_uint64: 1 << 40,
        optional_sint32: -100,
        optional_sint64: i64::MAX,
        optional_fixed32: 9,
        optional_fixed64: 10,
        optional_sfixed32: -11,
        optional_sfixed64: -12,
        optional_float: -0.0,
        optional_double: f64::INFINITY,
        optional_bool: true,
        optional_string: "Gióng".into(),
        optional_bytes: vec![0, 255, 7],
        optional_nested_message: Some(Box::new(p3::test_all_types_proto3::NestedMessage {
            a: 7,
            corecursive: None,
        })),
        optional_nested_enum: p3::test_all_types_proto3::NestedEnum::Baz as i32,
        optional_foreign_enum: 42,
        recursive_message: Some(Box::new(p3::TestAllTypesProto3 {
            optional_int32: -5,
            optional_string: "inner".into(),
            ..Default::default()
        })),
        repeated_int32: vec![1, -1, 300],
        repeated_string: vec!["a".into(), "".into(), "ccc".into()],
        repeated_nested_message: vec![
            p3::test_all_types_proto3::NestedMessage {
                a: 1,
                corecursive: None,
            },
            p3::test_all_types_proto3::NestedMessage {
                a: -2,
                corecursive: None,
            },
        ],
        packed_sint64: vec![-3, 0, 3],
        unpacked_fixed32: vec![1, 2],
        map_string_nested_message: [(
            "k1".to_string(),
            p3::test_all_types_proto3::NestedMessage {
                a: 3,
                corecursive: None,
            },
        )]
        .into(),
        map_int32_int32: [(1, 2), (3, 4)].into(),
        optional_int32_wrapper: Some(0),
        oneof_field: Some(p3::test_all_types_proto3::OneofField::OneofString(
            "chosen".into(),
        )),
        ..Default::default()
    };
    let bytes = w.encode_to_vec().unwrap();
    assert_eq!(bytes.len(), w.encoded_len());
    assert_eq!(p3::TestAllTypesProto3::decode(&bytes[..]).unwrap(), p);

    // The view of those bytes reads the same values.
    let v = g3::TestAllTypesProto3View::decode(&bytes).unwrap();
    assert_eq!(v.optional_string(), "Gióng");
    assert_eq!(v.optional_float().to_bits(), (-0.0f32).to_bits());
    assert_eq!(v.optional_nested_message().unwrap().a(), 7);
    assert_eq!(v.optional_foreign_enum().raw(), 42);
    assert_eq!(v.optional_foreign_enum().known(), None);
    assert_eq!(v.recursive_message().unwrap().optional_string(), "inner");
    assert_eq!(v.repeated_string().to_vec(), ["a", "", "ccc"]);
    assert_eq!(
        v.repeated_nested_message()
            .iter()
            .map(|m| m.a())
            .collect::<Vec<_>>(),
        [1, -2]
    );
    assert_eq!(v.packed_sint64().to_vec(), [-3, 0, 3]);
    assert_eq!(v.map_int32_int32().get(&3), Some(4));
    assert_eq!(
        v.map_string_nested_message().get(&"k1").map(|m| m.a()),
        Some(3)
    );
    assert!(v.optional_int32_wrapper().is_some());
    assert!(matches!(
        v.oneof_field(),
        Some(g3::test_all_types_proto3::OneofFieldView::OneofString(
            "chosen"
        ))
    ));
    // Field by field again: the same bytes the message struct wrote.
    let mut canonical = Vec::new();
    v.encode_canonical(&mut canonical);
    assert_eq!(canonical, bytes);
}

#[test]
fn proto2_presence_groups_and_defaults() {
    let group = g2::test_all_types_proto2::Data {
        group_int32: Some(5),
        group_uint32: Some(6),
        ..Default::default()
    };
    let w = g2::TestAllTypesProto2 {
        optional_int32: Some(0), // present although zero: proto2 keeps it
        optional_string: Some(""),
        data: Some(&group),
        repeated_int32: &[1, 2], // proto2: not packed unless asked
        packed_int32: &[3, 4],
        ..Default::default()
    };
    let p = p2::TestAllTypesProto2 {
        optional_int32: Some(0),
        optional_string: Some(String::new()),
        data: Some(p2::test_all_types_proto2::Data {
            group_int32: Some(5),
            group_uint32: Some(6),
        }),
        repeated_int32: vec![1, 2],
        packed_int32: vec![3, 4],
        ..Default::default()
    };
    let bytes = w.encode_to_vec().unwrap();
    assert_eq!(p2::TestAllTypesProto2::decode(&bytes[..]).unwrap(), p);
    assert_eq!(bytes, p.encode_to_vec(), "same bytes as prost");

    let v = g2::TestAllTypesProto2View::decode(&bytes).unwrap();
    assert!(v.has_optional_int32());
    assert!(!v.has_optional_int64());
    assert_eq!(v.data().unwrap().group_uint32(), 6);
    // [default = ...] values for absent fields.
    assert_eq!(v.default_int32(), -123456789);
    assert_eq!(v.default_string(), "Rosebud");
    assert_eq!(v.default_double(), 7e22);
    assert_eq!(v.default_uint64(), 10123456789123456789);
    assert_eq!(v.default_bytes(), b"joshua");
    assert!(v.default_bool());
}
