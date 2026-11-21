//! Walks an input with the schema to find the places where giong-proto and
//! prost are meant to differ (see [`crate::diff::Divergence`]).

use std::collections::HashMap;
use std::sync::OnceLock;

use giong_proto::wire::{Reader, WireType};
use prost_types::field_descriptor_proto::{Label, Type};
use prost_types::{DescriptorProto, FieldDescriptorProto};

/// What the walk found.
#[derive(Debug, Default, Clone, Copy)]
pub struct Findings {
    /// A known field with a wire type it does not use.
    pub wire_mismatch: bool,
    /// A proto2 enum field with a number its enum does not define.
    pub closed_enum_unknown: bool,
}

pub(crate) struct Schema {
    pub(crate) messages: HashMap<String, DescriptorProto>,
    /// Enum full name -> (closed, defined numbers).
    pub(crate) enums: HashMap<String, (bool, Vec<i32>)>,
}

pub(crate) fn schema() -> &'static Schema {
    static S: OnceLock<Schema> = OnceLock::new();
    S.get_or_init(|| {
        let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/proto");
        let files = [
            "conformance.proto",
            "test_messages_proto3.proto",
            "test_messages_proto2.proto",
        ];
        let fds = protox::compile(files.iter().map(|f| format!("{dir}/{f}")), [dir])
            .expect("protos compile");
        let mut s = Schema {
            messages: HashMap::new(),
            enums: HashMap::new(),
        };
        for f in &fds.file {
            let closed = f.syntax() != "proto3";
            let prefix = if f.package().is_empty() {
                String::new()
            } else {
                format!(".{}", f.package())
            };
            for e in &f.enum_type {
                s.enums.insert(
                    format!("{prefix}.{}", e.name()),
                    (closed, e.value.iter().map(|v| v.number()).collect()),
                );
            }
            for m in &f.message_type {
                add(&mut s, &prefix, m, closed);
            }
        }
        s
    })
}

fn add(s: &mut Schema, prefix: &str, m: &DescriptorProto, closed: bool) {
    let full = format!("{prefix}.{}", m.name());
    for e in &m.enum_type {
        s.enums.insert(
            format!("{full}.{}", e.name()),
            (closed, e.value.iter().map(|v| v.number()).collect()),
        );
    }
    for n in &m.nested_type {
        add(s, &full, n, closed);
    }
    s.messages.insert(full, m.clone());
}

pub(crate) fn expected(f: &FieldDescriptorProto) -> (WireType, bool) {
    let packable = f.label() == Label::Repeated
        && !matches!(
            f.r#type(),
            Type::String | Type::Bytes | Type::Message | Type::Group
        );
    let w = match f.r#type() {
        Type::Double | Type::Fixed64 | Type::Sfixed64 => WireType::I64,
        Type::Float | Type::Fixed32 | Type::Sfixed32 => WireType::I32,
        Type::String | Type::Bytes | Type::Message => WireType::Len,
        Type::Group => WireType::StartGroup,
        _ => WireType::Varint,
    };
    (w, packable)
}

/// Walks `bytes` as the message `full` (e.g.
/// `.protobuf_test_messages.proto3.TestAllTypesProto3`).
pub fn walk(full: &str, bytes: &[u8]) -> Findings {
    let mut out = Findings::default();
    walk_into(schema(), full, bytes, &mut out, 0);
    out
}

fn walk_into(s: &Schema, full: &str, bytes: &[u8], out: &mut Findings, depth: u32) {
    let Some(m) = s.messages.get(full) else {
        return;
    };
    if depth > 120 {
        return;
    }
    let mut r = Reader::new(bytes);
    loop {
        let Ok(Some((num, wire))) = r.tag() else {
            return;
        };
        let field = m.field.iter().find(|f| f.number() as u32 == num);
        if let Some(f) = field {
            let (want, packable) = expected(f);
            if wire != want && !(packable && wire == WireType::Len) {
                out.wire_mismatch = true;
            }
        }
        let body_start = r.pos();
        if r.skip(num, wire, 200).is_err() {
            return;
        }
        let body = &bytes[body_start..r.pos()];
        let Some(f) = field else { continue };
        let (want, packable) = expected(f);
        if wire != want && !(packable && wire == WireType::Len) {
            continue;
        }
        match f.r#type() {
            Type::Message => {
                let mut b = Reader::new(body);
                if let Ok(inner) = b.bytes() {
                    walk_into(s, f.type_name(), inner, out, depth + 1);
                }
            }
            Type::Group => {
                // body = group contents + end tag; strip the end tag.
                let mut g = Reader::new(body);
                let mut end = 0;
                loop {
                    let before = g.pos();
                    match g.tag() {
                        Ok(Some((n, WireType::EndGroup))) if n == num => {
                            end = before;
                            break;
                        }
                        Ok(Some((n, w))) => {
                            if g.skip(n, w, 200).is_err() {
                                break;
                            }
                        }
                        _ => break,
                    }
                }
                walk_into(s, f.type_name(), &body[..end], out, depth + 1);
            }
            Type::Enum => {
                let Some((closed, values)) = s.enums.get(f.type_name()) else {
                    continue;
                };
                if !closed {
                    continue;
                }
                let mut v = Reader::new(body);
                if wire == WireType::Len {
                    let Ok(p) = v.bytes() else { continue };
                    let mut p = Reader::new(p);
                    while !p.is_empty() {
                        match p.varint() {
                            Ok(x) if !values.contains(&(x as i32)) => {
                                out.closed_enum_unknown = true
                            }
                            Ok(_) => {}
                            Err(_) => break,
                        }
                    }
                } else if let Ok(x) = v.varint() {
                    if !values.contains(&(x as i32)) {
                        out.closed_enum_unknown = true;
                    }
                }
            }
            _ => {}
        }
    }
}
