//! # giong-grpc-build
//!
//! Generates Rust code for [`giong-proto`] messages and [`giong-grpc`]
//! services from `.proto` files at build time. The `.proto` files are
//! parsed in Rust by [protox], so there is no `protoc` binary to install.
//!
//! In `build.rs`:
//!
//! ```no_run
//! giong_grpc_build::compile(&["proto/helloworld.proto"], &["proto"]).unwrap();
//! ```
//!
//! and in the crate:
//!
//! ```ignore
//! mod pb {
//!     include!(concat!(env!("OUT_DIR"), "/giong_proto.rs"));
//! }
//! use giong_proto::View;
//! let req = pb::helloworld::HelloRequestView::decode(&buf)?;
//! ```
//!
//! Every package becomes a module (`foo.bar` is `foo::bar`), every message
//! `M` a view type `MView<'a>` that borrows from the input and a struct
//! `M<'a>` to build and encode, its nested types a module `m`, and every
//! enum a Rust enum. Every service `S` becomes a module `s` with the paths
//! and kinds of its methods, a trait `S` with one method for each rpc, a
//! type `SServer` that routes calls to an implementation of the trait
//! (what `giong_grpc::Server::add` takes), and a client `SClient`.
//! [`Config::services`] leaves the trait, the server and the client out,
//! for a crate that only needs the messages.
//!
//! The file starts with `FILE_DESCRIPTOR_SET`: the `.proto` files in
//! encoded form, for `giong_grpc::reflection`.
//!
//! The output is one file of plain Rust: no macros, nothing hidden.
//!
//! [`giong-proto`]: https://docs.rs/giong-proto
//! [`giong-grpc`]: https://docs.rs/giong-grpc
//! [protox]: https://docs.rs/protox

#![warn(missing_docs)]

use std::collections::{BTreeMap, HashMap};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use prost_types::field_descriptor_proto::{Label, Type};
use prost_types::{DescriptorProto, EnumDescriptorProto, FieldDescriptorProto, FileDescriptorSet};

/// Compiles `files` (searched for in `includes`) and writes
/// `$OUT_DIR/giong_proto.rs`, with everything [`Config`] generates by
/// default. Prints the `cargo:rerun-if-changed` lines for the files it
/// read.
pub fn compile(
    files: &[impl AsRef<Path>],
    includes: &[impl AsRef<Path>],
) -> Result<(), Box<dyn std::error::Error>> {
    Config::new().compile(files, includes)
}

/// What to generate, for a build that needs something other than
/// [`compile`]'s defaults.
///
/// ```no_run
/// // Messages only: the crate does not depend on giong-grpc.
/// giong_grpc_build::Config::new()
///     .services(false)
///     .compile(&["proto/events.proto"], &["proto"])
///     .unwrap();
/// ```
#[derive(Debug, Clone)]
#[must_use = "a configuration does nothing until it compiles"]
pub struct Config {
    services: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self::new()
    }
}

impl Config {
    /// The defaults: messages, enums, and for each service its trait, its
    /// server and its client.
    pub fn new() -> Self {
        Self { services: true }
    }

    /// Whether each service gets a trait to implement, a `...Server` that
    /// routes calls to it and a `...Client` (`true` by default). That code
    /// uses the giong-grpc crate; without it, a service is only described
    /// (its name, and the path and kind of each method).
    pub fn services(mut self, on: bool) -> Self {
        self.services = on;
        self
    }

    /// Compiles `files` (searched for in `includes`) and writes
    /// `$OUT_DIR/giong_proto.rs`. Prints the `cargo:rerun-if-changed`
    /// lines for the files it read.
    pub fn compile(
        &self,
        files: &[impl AsRef<Path>],
        includes: &[impl AsRef<Path>],
    ) -> Result<(), Box<dyn std::error::Error>> {
        let out = PathBuf::from(
            std::env::var_os("OUT_DIR").ok_or("OUT_DIR is not set: call compile from build.rs")?,
        );
        self.compile_to(files, includes, &out.join("giong_proto.rs"))
    }

    /// Like [`compile`](Self::compile), writing to `out` instead.
    pub fn compile_to(
        &self,
        files: &[impl AsRef<Path>],
        includes: &[impl AsRef<Path>],
        out: &Path,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let fds = protox::compile(files, includes)?;
        for f in files {
            println!("cargo:rerun-if-changed={}", f.as_ref().display());
        }
        std::fs::write(out, self.generate(&fds))?;
        Ok(())
    }

    /// Generates the Rust code for every file in `fds` (imports included),
    /// as one source text.
    pub fn generate(&self, fds: &FileDescriptorSet) -> String {
        let index = Index::new(fds);
        let mut tree = Module::default();
        for file in &fds.file {
            let pkg: Vec<String> = file
                .package()
                .split('.')
                .filter(|s| !s.is_empty())
                .map(module_name)
                .collect();
            let ctx = FileCtx {
                proto2: file.syntax() != "proto3" && file.syntax() != "editions",
            };
            let m = tree.at(&pkg);
            for e in &file.enum_type {
                gen_enum(&mut m.code, e);
            }
            for msg in &file.message_type {
                let prefix = if file.package().is_empty() {
                    String::new()
                } else {
                    format!(".{}", file.package())
                };
                gen_message(&index, &ctx, m, &pkg, &prefix, msg);
            }
            for s in &file.service {
                gen_service(&index, &mut m.code, &pkg, file.package(), s, self.services);
            }
        }
        let mut out = String::from("// Generated by giong-grpc-build. Do not edit.\n\n");
        gen_descriptor_set(&mut out, fds);
        tree.write(&mut out);
        out
    }
}

/// The files as a constant, for reflection: comments and positions
/// (`source_code_info`) are left out, which more than halves it.
fn gen_descriptor_set(out: &mut String, fds: &FileDescriptorSet) {
    use prost::Message as _;

    let mut fds = fds.clone();
    for file in &mut fds.file {
        file.source_code_info = None;
    }
    out.push_str(
        "/// The `.proto` files this code was generated from, their imports\n\
         /// included, as an encoded `google.protobuf.FileDescriptorSet`: what\n\
         /// `giong_grpc::reflection::Reflection::new` takes. A program that\n\
         /// does not use it does not carry it.\n\
         #[allow(dead_code)]\n\
         pub const FILE_DESCRIPTOR_SET: &[u8] = b\"\\\n",
    );
    for line in fds.encode_to_vec().chunks(24) {
        out.push_str("    ");
        for &b in line {
            // A trailing space would be eaten by the line continuation.
            match b {
                b'"' | b'\\' => {
                    out.push('\\');
                    out.push(b as char);
                }
                0x21..=0x7e => out.push(b as char),
                _ => {
                    let _ = write!(out, "\\x{b:02x}");
                }
            }
        }
        out.push_str("\\\n");
    }
    out.push_str("    \";\n\n");
}

#[derive(Default)]
struct Module {
    code: String,
    children: BTreeMap<String, Module>,
}

impl Module {
    fn at(&mut self, path: &[String]) -> &mut Module {
        let mut m = self;
        for p in path {
            m = m.children.entry(p.clone()).or_default();
        }
        m
    }

    fn write(&self, out: &mut String) {
        out.push_str(&self.code);
        for (name, child) in &self.children {
            let _ = writeln!(out, "pub mod {name} {{");
            let _ = writeln!(out, "#![allow(clippy::all, non_camel_case_types, non_snake_case, non_upper_case_globals, dead_code, unused_imports, unused_variables, unused_mut)]");
            child.write(out);
            out.push_str("}\n");
        }
    }
}

/// What the generator knows about each named type.
struct TypeInfo {
    /// Modules from the root of the generated tree.
    path: Vec<String>,
    /// The Rust type name (without `View`).
    name: String,
    /// Enums only: defined in a proto2 file, so closed.
    closed: bool,
    /// Enums only: the Rust variant of the first value.
    first_variant: String,
    map_entry: Option<(FieldDescriptorProto, FieldDescriptorProto)>,
    /// Messages only: the message types held inline (singular and oneof
    /// message fields), for finding cycles.
    inline: Vec<String>,
}

struct Index {
    types: HashMap<String, TypeInfo>,
    /// (enum full name, proto value name) -> Rust variant (aliases map to
    /// the variant that holds their number).
    enum_values: HashMap<(String, String), String>,
}

impl Index {
    fn new(fds: &FileDescriptorSet) -> Index {
        let mut types = HashMap::new();
        for file in &fds.file {
            let pkg: Vec<String> = file
                .package()
                .split('.')
                .filter(|s| !s.is_empty())
                .map(module_name)
                .collect();
            let prefix = if file.package().is_empty() {
                String::new()
            } else {
                format!(".{}", file.package())
            };
            let closed = file.syntax() != "proto3" && file.syntax() != "editions";
            for e in &file.enum_type {
                index_enum(&mut types, &pkg, &prefix, e, closed);
            }
            for m in &file.message_type {
                index_message(&mut types, &pkg, &prefix, m, closed);
            }
        }
        let mut enum_values = HashMap::new();
        for file in &fds.file {
            let prefix = if file.package().is_empty() {
                String::new()
            } else {
                format!(".{}", file.package())
            };
            record_all_enums(
                &mut enum_values,
                &prefix,
                &file.enum_type,
                &file.message_type,
            );
        }
        Index { types, enum_values }
    }

    fn get(&self, full: &str) -> &TypeInfo {
        self.types
            .get(full)
            .unwrap_or_else(|| panic!("giong-grpc-build: unknown type {full}"))
    }

    /// Whether `from` can reach `to` through inline message fields.
    fn reaches(&self, from: &str, to: &str) -> bool {
        let mut seen = std::collections::HashSet::new();
        let mut stack = vec![from.to_string()];
        while let Some(t) = stack.pop() {
            if t == to {
                return true;
            }
            if !seen.insert(t.clone()) {
                continue;
            }
            if let Some(info) = self.types.get(&t) {
                stack.extend(info.inline.iter().cloned());
            }
        }
        false
    }
}

fn index_enum(
    types: &mut HashMap<String, TypeInfo>,
    path: &[String],
    prefix: &str,
    e: &EnumDescriptorProto,
    closed: bool,
) {
    let first = e
        .value
        .first()
        .map(|v| {
            variant_names(e)[0]
                .clone()
                .unwrap_or_else(|| v.name().to_string())
        })
        .unwrap_or_default();
    types.insert(
        format!("{prefix}.{}", e.name()),
        TypeInfo {
            path: path.to_vec(),
            name: type_name(e.name()),
            closed,
            first_variant: first,
            map_entry: None,
            inline: vec![],
        },
    );
}

fn index_message(
    types: &mut HashMap<String, TypeInfo>,
    path: &[String],
    prefix: &str,
    m: &DescriptorProto,
    closed: bool,
) {
    let full = format!("{prefix}.{}", m.name());
    let map_entry = if m
        .options
        .as_ref()
        .and_then(|o| o.map_entry)
        .unwrap_or(false)
    {
        Some((m.field[0].clone(), m.field[1].clone()))
    } else {
        None
    };
    let inline = m
        .field
        .iter()
        .filter(|f| {
            f.label() != Label::Repeated && matches!(f.r#type(), Type::Message | Type::Group)
        })
        .map(|f| f.type_name().to_string())
        .collect();
    types.insert(
        full.clone(),
        TypeInfo {
            path: path.to_vec(),
            name: type_name(m.name()),
            closed,
            first_variant: String::new(),
            map_entry,
            inline,
        },
    );
    let mut sub = path.to_vec();
    sub.push(module_name(m.name()));
    for e in &m.enum_type {
        index_enum(types, &sub, &full, e, closed);
    }
    for n in &m.nested_type {
        index_message(types, &sub, &full, n, closed);
    }
}

struct FileCtx {
    proto2: bool,
}

fn record_all_enums(
    map: &mut HashMap<(String, String), String>,
    prefix: &str,
    enums: &[EnumDescriptorProto],
    msgs: &[DescriptorProto],
) {
    for e in enums {
        let full = format!("{prefix}.{}", e.name());
        let mut by_num = HashMap::new();
        for (v, n) in e.value.iter().zip(variant_names(e)) {
            if let Some(n) = n {
                by_num.insert(v.number(), n);
            }
        }
        for v in &e.value {
            if let Some(n) = by_num.get(&v.number()) {
                map.insert((full.clone(), v.name().to_string()), n.clone());
            }
        }
    }
    for m in msgs {
        record_all_enums(
            map,
            &format!("{prefix}.{}", m.name()),
            &m.enum_type,
            &m.nested_type,
        );
    }
}

/// A path to `info`'s Rust type from code in module `from`.
fn rel_path(from: &[String], info: &TypeInfo) -> String {
    let mut s = String::new();
    for _ in from {
        s.push_str("super::");
    }
    for p in &info.path {
        s.push_str(p);
        s.push_str("::");
    }
    s
}

/// How a field of a view is stored and read.
struct FieldGen {
    /// Field number.
    num: u32,
    /// Rust identifier for the struct field and accessor.
    ident: String,
    /// The `giong_proto::view` kind type (`Int32`, `Message<FooView<'a>>`, ...).
    kind: String,
    /// The value type an accessor returns for one value.
    value: String,
    /// Wire type constant for one value.
    wire: &'static str,
    /// Scalar, string or bytes (not a message or group).
    scalar: bool,
    float: bool,
    string_like: bool,
    group: bool,
    closed_enum: Option<String>,
    /// Message fields held through `Lazy` because of a cycle.
    lazy: bool,
    packed: bool,
    /// Singular scalar with explicit presence (proto2, proto3 `optional`).
    explicit: bool,
}

const KEYWORDS: &[&str] = &[
    "as", "break", "const", "continue", "else", "enum", "extern", "false", "fn", "for", "if",
    "impl", "in", "let", "loop", "match", "mod", "move", "mut", "pub", "ref", "return", "static",
    "struct", "trait", "true", "type", "unsafe", "use", "where", "while", "async", "await", "dyn",
    "abstract", "become", "box", "do", "final", "macro", "override", "priv", "typeof", "unsized",
    "virtual", "yield", "try", "gen",
];

fn ident(name: &str) -> String {
    match name {
        "self" | "super" | "crate" | "Self" => format!("{name}_"),
        n if KEYWORDS.contains(&n) => format!("r#{n}"),
        n => n.to_string(),
    }
}

/// `has_` + name, without a raw prefix.
fn has_ident(id: &str) -> String {
    format!("has_{}", id.trim_start_matches("r#"))
}

fn snake(name: &str) -> String {
    let mut out = String::new();
    let chars: Vec<char> = name.chars().collect();
    for (i, &c) in chars.iter().enumerate() {
        if c.is_ascii_uppercase() {
            let prev_lower =
                i > 0 && (chars[i - 1].is_ascii_lowercase() || chars[i - 1].is_ascii_digit());
            let next_lower = i + 1 < chars.len() && chars[i + 1].is_ascii_lowercase();
            let prev_upper = i > 0 && chars[i - 1].is_ascii_uppercase();
            if i > 0 && (prev_lower || (prev_upper && next_lower)) && !out.ends_with('_') {
                out.push('_');
            }
            out.push(c.to_ascii_lowercase());
        } else {
            out.push(c);
        }
    }
    out
}

fn module_name(name: &str) -> String {
    ident(&snake(name))
}

fn camel(name: &str) -> String {
    let mut out = String::new();
    let mut up = true;
    for c in name.chars() {
        if c == '_' {
            up = true;
        } else if up {
            out.push(c.to_ascii_uppercase());
            up = false;
        } else {
            out.push(c);
        }
    }
    out
}

fn type_name(name: &str) -> String {
    match name {
        "Self" => "Self_".into(),
        n => n.to_string(),
    }
}

/// The Rust variant name for each value of `e`; `None` for a value whose
/// number an earlier value already took (an alias).
fn variant_names(e: &EnumDescriptorProto) -> Vec<Option<String>> {
    let prefix = snake(e.name()).to_ascii_uppercase() + "_";
    let mut seen_num = std::collections::HashSet::new();
    let mut seen_name = std::collections::HashSet::new();
    e.value
        .iter()
        .map(|v| {
            if !seen_num.insert(v.number()) {
                return None;
            }
            let raw = v.name();
            let stripped = raw
                .strip_prefix(&prefix)
                .filter(|s| !s.is_empty() && !s.starts_with(|c: char| c.is_ascii_digit()))
                .unwrap_or(raw);
            let mut name = camel(&stripped.to_ascii_lowercase());
            if name.is_empty() || name.starts_with(|c: char| c.is_ascii_digit()) {
                name = format!("V{name}");
            }
            if name == "Self" {
                name = "Self_".into();
            }
            while !seen_name.insert(name.clone()) {
                name.push('_');
            }
            Some(name)
        })
        .collect()
}

fn gen_enum(out: &mut String, e: &EnumDescriptorProto) {
    let name = type_name(e.name());
    let names = variant_names(e);
    let _ = writeln!(out, "/// The protobuf enum `{}`.", e.name());
    let _ = writeln!(
        out,
        "#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]\n#[repr(i32)]\npub enum {name} {{"
    );
    for (v, n) in e.value.iter().zip(&names) {
        if let Some(n) = n {
            let _ = writeln!(out, "    /// `{}`", v.name());
            let _ = writeln!(out, "    {n} = {},", v.number());
        }
    }
    out.push_str("}\n");
    let _ = writeln!(out, "impl giong_proto::EnumType for {name} {{");
    out.push_str("    #[inline]\n    fn from_i32(v: i32) -> Option<Self> {\n        match v {\n");
    for (v, n) in e.value.iter().zip(&names) {
        if let Some(n) = n {
            let _ = writeln!(out, "            {} => Some(Self::{n}),", v.number());
        }
    }
    out.push_str("            _ => None,\n        }\n    }\n    #[inline]\n    fn to_i32(self) -> i32 {\n        self as i32\n    }\n}\n");
    let _ = writeln!(
        out,
        "impl {name} {{\n    /// Whether `v` is one of the enum's numbers.\n    #[inline]\n    pub fn is_known(v: i32) -> bool {{\n        <Self as giong_proto::EnumType>::from_i32(v).is_some()\n    }}\n}}"
    );
}

fn field_gen(
    index: &Index,
    ctx: &FileCtx,
    msg_full: &str,
    from: &[String],
    f: &FieldDescriptorProto,
) -> FieldGen {
    let (kind, value, wire, scalar, float, string_like, packable): (
        String,
        String,
        &'static str,
        bool,
        bool,
        bool,
        bool,
    ) = match f.r#type() {
        Type::Int32 => (
            "Int32".into(),
            "i32".into(),
            "Varint",
            true,
            false,
            false,
            true,
        ),
        Type::Int64 => (
            "Int64".into(),
            "i64".into(),
            "Varint",
            true,
            false,
            false,
            true,
        ),
        Type::Uint32 => (
            "UInt32".into(),
            "u32".into(),
            "Varint",
            true,
            false,
            false,
            true,
        ),
        Type::Uint64 => (
            "UInt64".into(),
            "u64".into(),
            "Varint",
            true,
            false,
            false,
            true,
        ),
        Type::Sint32 => (
            "SInt32".into(),
            "i32".into(),
            "Varint",
            true,
            false,
            false,
            true,
        ),
        Type::Sint64 => (
            "SInt64".into(),
            "i64".into(),
            "Varint",
            true,
            false,
            false,
            true,
        ),
        Type::Bool => (
            "Bool".into(),
            "bool".into(),
            "Varint",
            true,
            false,
            false,
            true,
        ),
        Type::Fixed32 => (
            "Fixed32".into(),
            "u32".into(),
            "I32",
            true,
            false,
            false,
            true,
        ),
        Type::Fixed64 => (
            "Fixed64".into(),
            "u64".into(),
            "I64",
            true,
            false,
            false,
            true,
        ),
        Type::Sfixed32 => (
            "SFixed32".into(),
            "i32".into(),
            "I32",
            true,
            false,
            false,
            true,
        ),
        Type::Sfixed64 => (
            "SFixed64".into(),
            "i64".into(),
            "I64",
            true,
            false,
            false,
            true,
        ),
        Type::Float => ("Float".into(), "f32".into(), "I32", true, true, false, true),
        Type::Double => (
            "Double".into(),
            "f64".into(),
            "I64",
            true,
            true,
            false,
            true,
        ),
        Type::String => (
            "Str".into(),
            "&'a str".into(),
            "Len",
            true,
            false,
            true,
            false,
        ),
        Type::Bytes => (
            "Bytes".into(),
            "&'a [u8]".into(),
            "Len",
            true,
            false,
            true,
            false,
        ),
        Type::Enum => {
            let info = index.get(f.type_name());
            let path = format!("{}{}", rel_path(from, info), info.name);
            let k = if info.closed {
                format!("ClosedEnum<{path}>")
            } else {
                format!("OpenEnum<{path}>")
            };
            (
                k,
                format!("giong_proto::Enum<{path}>"),
                "Varint",
                true,
                false,
                false,
                true,
            )
        }
        Type::Message | Type::Group => {
            let info = index.get(f.type_name());
            let path = format!("{}{}View<'a>", rel_path(from, info), info.name);
            let group = f.r#type() == Type::Group;
            let k = if group {
                format!("Group<{path}>")
            } else {
                format!("Message<{path}>")
            };
            (
                k,
                path,
                if group { "StartGroup" } else { "Len" },
                false,
                false,
                false,
                false,
            )
        }
    };
    let closed_enum = if f.r#type() == Type::Enum && index.get(f.type_name()).closed {
        let info = index.get(f.type_name());
        Some(format!("{}{}", rel_path(from, info), info.name))
    } else {
        None
    };
    let repeated = f.label() == Label::Repeated;
    let packed = repeated
        && packable
        && match f.options.as_ref().and_then(|o| o.packed) {
            Some(p) => p,
            None => !ctx.proto2,
        };
    let lazy = !scalar && !repeated && index.reaches(f.type_name(), msg_full);
    FieldGen {
        num: f.number() as u32,
        ident: ident(f.name()),
        kind: format!("giong_proto::view::{kind}"),
        value,
        wire,
        scalar,
        float,
        string_like,
        group: f.r#type() == Type::Group,
        closed_enum,
        lazy,
        packed,
        explicit: f.proto3_optional()
            || f.label() == Label::Required
            || (f.label() == Label::Optional && ctx.proto2),
    }
}

/// The Rust expression for a proto2 `[default = ...]`, or `None` to use the
/// type's own default.
fn default_expr(index: &Index, from: &[String], f: &FieldDescriptorProto) -> Option<String> {
    let d = f.default_value.as_deref();
    match f.r#type() {
        Type::Enum => {
            let info = index.get(f.type_name());
            let path = format!("{}{}", rel_path(from, info), info.name);
            let variant = match d {
                Some(name) => {
                    // Find the variant for the value's proto name.
                    let names = index
                        .enum_values
                        .get(&(f.type_name().to_string(), name.to_string()))
                        .cloned();
                    names.unwrap_or_else(|| info.first_variant.clone())
                }
                None => info.first_variant.clone(),
            };
            if variant.is_empty() {
                None
            } else {
                Some(format!(
                    "giong_proto::Enum::from_raw({path}::{variant} as i32)"
                ))
            }
        }
        _ => {
            let d = d?;
            Some(match f.r#type() {
                Type::Float | Type::Double => {
                    let t = if f.r#type() == Type::Float {
                        "f32"
                    } else {
                        "f64"
                    };
                    match d {
                        "inf" => format!("{t}::INFINITY"),
                        "-inf" => format!("{t}::NEG_INFINITY"),
                        "nan" => format!("{t}::NAN"),
                        v => format!("{v}_{t}"),
                    }
                }
                Type::String => format!("{d:?}"),
                Type::Bytes => {
                    let mut lit = String::from("b\"");
                    for b in unescape_c(d) {
                        let _ = write!(lit, "\\x{b:02x}");
                    }
                    lit.push('"');
                    lit
                }
                Type::Bool => d.to_string(),
                Type::Int32 | Type::Sint32 | Type::Sfixed32 => format!("{d}_i32"),
                Type::Int64 | Type::Sint64 | Type::Sfixed64 => format!("{d}_i64"),
                Type::Uint32 | Type::Fixed32 => format!("{d}_u32"),
                Type::Uint64 | Type::Fixed64 => format!("{d}_u64"),
                _ => return None,
            })
        }
    }
}

/// Decodes the C escapes protoc uses for `bytes` defaults.
fn unescape_c(s: &str) -> Vec<u8> {
    let b = s.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        if b[i] != b'\\' || i + 1 == b.len() {
            out.push(b[i]);
            i += 1;
            continue;
        }
        i += 1;
        match b[i] {
            b'n' => out.push(b'\n'),
            b'r' => out.push(b'\r'),
            b't' => out.push(b'\t'),
            b'0'..=b'7' => {
                let mut v = 0u32;
                let mut n = 0;
                while n < 3 && i < b.len() && (b'0'..=b'7').contains(&b[i]) {
                    v = v * 8 + (b[i] - b'0') as u32;
                    i += 1;
                    n += 1;
                }
                out.push(v as u8);
                continue;
            }
            b'x' => {
                let mut v = 0u32;
                i += 1;
                let mut n = 0;
                while n < 2 && i < b.len() && b[i].is_ascii_hexdigit() {
                    v = v * 16 + (b[i] as char).to_digit(16).unwrap();
                    i += 1;
                    n += 1;
                }
                out.push(v as u8);
                continue;
            }
            c => out.push(c),
        }
        i += 1;
    }
    out
}

fn gen_message(
    index: &Index,
    ctx: &FileCtx,
    parent: &mut Module,
    path: &[String],
    prefix: &str,
    msg: &DescriptorProto,
) {
    let full = format!("{prefix}.{}", msg.name());
    if msg
        .options
        .as_ref()
        .and_then(|o| o.map_entry)
        .unwrap_or(false)
    {
        return; // Map entries use giong_proto::view::MapEntry.
    }
    let name = type_name(msg.name());
    let view = format!("{name}View");
    let modname = module_name(msg.name());
    let mut sub_path = path.to_vec();
    sub_path.push(modname.clone());

    // Nested types first, in their module.
    {
        let sub = parent.children.entry(modname.clone()).or_default();
        for e in &msg.enum_type {
            gen_enum(&mut sub.code, e);
        }
    }
    for n in &msg.nested_type {
        let sub = parent.children.entry(modname.clone()).or_default();
        gen_message(index, ctx, sub, &sub_path, &full, n);
    }

    // Real oneofs (proto3 `optional` makes a synthetic one, not a oneof).
    let mut oneofs: Vec<(String, Vec<&FieldDescriptorProto>)> = msg
        .oneof_decl
        .iter()
        .map(|o| (o.name().to_string(), vec![]))
        .collect();
    for f in &msg.field {
        if let Some(i) = f.oneof_index {
            if !f.proto3_optional() {
                oneofs[i as usize].1.push(f);
            }
        }
    }
    let in_oneof = |f: &FieldDescriptorProto| f.oneof_index.is_some() && !f.proto3_optional();

    let mut out = String::new();
    let mut fields: Vec<&FieldDescriptorProto> = msg.field.iter().collect();
    fields.sort_by_key(|f| f.number());

    // The struct.
    let _ = writeln!(
        out,
        "/// A view of the message `{}`, borrowing from the buffer it was decoded from.",
        &full[1..]
    );
    let _ = writeln!(out, "#[derive(Clone, Default)]\npub struct {view}<'a> {{");
    for f in &fields {
        if in_oneof(f) {
            continue;
        }
        let g = field_gen(index, ctx, &full, path, f);
        let ty = storage_type(index, &g, f, path);
        let _ = writeln!(out, "    {}: {ty},", g.ident);
    }
    for (oname, members) in &oneofs {
        if members.is_empty() {
            continue;
        }
        let _ = writeln!(
            out,
            "    {}: Option<{modname}::{}View<'a>>,",
            ident(oname),
            camel(oname)
        );
    }
    out.push_str("    __giong_src: giong_proto::view::Spans<'a>,\n    __giong_unknown: bool,\n}\n");

    // Accessors.
    let _ = writeln!(out, "impl<'a> {view}<'a> {{");
    for f in &fields {
        if in_oneof(f) {
            continue;
        }
        let g = field_gen(index, ctx, &full, path, f);
        gen_accessor(index, &mut out, &g, f, path, ctx);
    }
    for (oname, members) in &oneofs {
        if members.is_empty() {
            continue;
        }
        let id = ident(oname);
        let _ = writeln!(
            out,
            "    /// The oneof `{oname}`: which field is set, or `None`.\n    #[inline]\n    pub fn {id}(&self) -> Option<&{modname}::{}View<'a>> {{\n        self.{id}.as_ref()\n    }}",
            camel(oname)
        );
    }
    let has_decode_clash = fields.iter().any(|f| {
        matches!(
            f.name(),
            "decode" | "merge" | "encode" | "encoded_len" | "encode_to_vec"
        )
    });
    if !has_decode_clash {
        let _ = writeln!(
            out,
            "    /// Decodes the message from `buf`, checking all of it. The view borrows from `buf`.\n    #[inline]\n    pub fn decode(buf: &'a [u8]) -> Result<Self, giong_proto::Error> {{\n        <Self as giong_proto::View<'a>>::decode(buf)\n    }}"
        );
    }
    // Field classes for the unknown-field pass.
    out.push_str("    fn __giong_class(field: u32, wire: giong_proto::wire::WireType) -> giong_proto::view::FieldClass {\n        use giong_proto::view::FieldClass;\n        use giong_proto::wire::WireType;\n        match (field, wire) {\n");
    for f in &fields {
        let g = field_gen(index, ctx, &full, path, f);
        let repeated = f.label() == Label::Repeated;
        let packable = repeated && g.scalar && !g.string_like;
        let pat = if packable {
            format!("({}, WireType::{} | WireType::Len)", g.num, g.wire)
        } else {
            format!("({}, WireType::{})", g.num, g.wire)
        };
        match &g.closed_enum {
            Some(e) => {
                let _ = writeln!(
                    out,
                    "            {pat} => FieldClass::ClosedEnum({e}::is_known),"
                );
            }
            None => {
                let _ = writeln!(out, "            {pat} => FieldClass::Known,");
            }
        }
    }
    out.push_str("            _ => FieldClass::Unknown,\n        }\n    }\n}\n");

    // View: merge.
    let _ = writeln!(out, "impl<'a> giong_proto::View<'a> for {view}<'a> {{");
    out.push_str("    #[inline]\n    fn merge(&mut self, buf: &'a [u8], depth: u32) -> Result<(), giong_proto::Error> {\n        use giong_proto::view::Kind;\n        use giong_proto::wire::WireType;\n        self.__giong_src.push(buf);\n        let mut r = giong_proto::wire::Reader::new(buf);\n        loop {\n            let start = r.pos();\n            let Some((field, wire)) = r.tag()? else { break };\n            match (field, wire) {\n");
    for f in &fields {
        let g = field_gen(index, ctx, &full, path, f);
        gen_merge_arm(index, &mut out, &g, f, &modname, &oneofs, in_oneof(f));
    }
    out.push_str("                (f, w) => {\n                    self.__giong_unknown = true;\n                    r.skip(f, w, depth)?\n                }\n            }\n        }\n        Ok(())\n    }\n");

    // Canonical encoding, field by field.
    out.push_str("    fn canonical_len(&self) -> usize {\n        use giong_proto::view::Kind;\n        let mut n = 0;\n");
    let mut done_oneofs = std::collections::HashSet::new();
    for f in &fields {
        if in_oneof(f) {
            let oi = f.oneof_index.unwrap() as usize;
            if done_oneofs.insert(oi) {
                gen_oneof_encode(
                    index,
                    ctx,
                    &full,
                    path,
                    &mut out,
                    &modname,
                    &oneofs[oi],
                    true,
                );
            }
            continue;
        }
        let g = field_gen(index, ctx, &full, path, f);
        gen_encode_field(&mut out, &g, f, ctx, true);
    }
    out.push_str("        if self.__giong_unknown {\n            n += giong_proto::view::unknown_len(&self.__giong_src, Self::__giong_class);\n        }\n        n\n    }\n");
    out.push_str("    fn encode_canonical(&self, buf: &mut Vec<u8>) {\n        use giong_proto::view::Kind;\n");
    let mut done_oneofs = std::collections::HashSet::new();
    for f in &fields {
        if in_oneof(f) {
            let oi = f.oneof_index.unwrap() as usize;
            if done_oneofs.insert(oi) {
                gen_oneof_encode(
                    index,
                    ctx,
                    &full,
                    path,
                    &mut out,
                    &modname,
                    &oneofs[oi],
                    false,
                );
            }
            continue;
        }
        let g = field_gen(index, ctx, &full, path, f);
        gen_encode_field(&mut out, &g, f, ctx, false);
    }
    out.push_str("        if self.__giong_unknown {\n            giong_proto::view::put_unknown(&self.__giong_src, Self::__giong_class, buf);\n        }\n    }\n}\n");
    let _ = writeln!(
        out,
        "impl<'a> giong_proto::Encode for {view}<'a> {{\n    /// The bytes the view was decoded from.\n    #[inline]\n    fn encoded_len(&self) -> usize {{\n        giong_proto::view::spans_len(&self.__giong_src)\n    }}\n    /// Appends the bytes the view was decoded from, unchanged.\n    #[inline]\n    fn encode_raw(&self, buf: &mut Vec<u8>) {{\n        giong_proto::view::put_spans(&self.__giong_src, buf)\n    }}\n}}"
    );

    // Debug.
    let _ = writeln!(out, "impl<'a> std::fmt::Debug for {view}<'a> {{\n    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {{\n        let mut d = f.debug_struct(\"{name}\");");
    for f in &fields {
        if in_oneof(f) {
            continue;
        }
        let g = field_gen(index, ctx, &full, path, f);
        let _ = writeln!(out, "        d.field(\"{}\", &self.{});", f.name(), g.ident);
    }
    for (oname, members) in &oneofs {
        if !members.is_empty() {
            let _ = writeln!(out, "        d.field(\"{oname}\", &self.{});", ident(oname));
        }
    }
    out.push_str("        d.finish()\n    }\n}\n");

    // Oneof enums in the message's module.
    let mut sub_code = String::new();
    for (oname, members) in &oneofs {
        if members.is_empty() {
            continue;
        }
        let _ = writeln!(
            sub_code,
            "/// The oneof `{oname}` of `{}`.\n#[derive(Clone, Debug)]\npub enum {}View<'a> {{",
            &full[1..],
            camel(oname)
        );
        for f in members {
            let g = field_gen(index, ctx, &full, &sub_path, f);
            let ty = if g.lazy {
                format!("giong_proto::Lazy<'a, {}>", g.value)
            } else {
                g.value.clone()
            };
            let _ = writeln!(
                sub_code,
                "    /// Field `{}` ({}).\n    {}({ty}),",
                f.name(),
                g.num,
                camel(f.name())
            );
        }
        sub_code
            .push_str("    #[doc(hidden)]\n    __Phantom(std::marker::PhantomData<&'a ()>),\n}\n");
    }
    let sub = parent.children.entry(modname).or_default();
    sub.code.push_str(&sub_code);

    parent.code.push_str(&out);
    gen_writer(index, ctx, parent, path, &full, msg, &oneofs);
    let _ = writeln!(
        parent.code,
        "impl giong_proto::MessageType for {name}<'static> {{\n    const NAME: &'static str = \"{}\";\n    type View<'a> = {view}<'a>;\n}}\nimpl<'a> giong_proto::MessageOf for {name}<'a> {{\n    type Type = {name}<'static>;\n}}\nimpl<'a> giong_proto::MessageOf for {view}<'a> {{\n    type Type = {name}<'static>;\n}}",
        &full[1..]
    );
}

fn storage_type(index: &Index, g: &FieldGen, f: &FieldDescriptorProto, path: &[String]) -> String {
    if f.label() == Label::Repeated {
        if let Some((k, v)) = map_entry(index, f) {
            let kg = map_kind(index, path, &k);
            let vg = map_kind(index, path, &v);
            return format!("giong_proto::Map<'a, {kg}, {vg}>");
        }
        return format!("giong_proto::Repeated<'a, {}>", g.kind);
    }
    if g.lazy {
        return format!("giong_proto::Lazy<'a, {}>", g.value);
    }
    if !g.scalar || g.explicit {
        return format!("Option<{}>", g.value);
    }
    g.value.clone()
}

fn map_entry(
    index: &Index,
    f: &FieldDescriptorProto,
) -> Option<(FieldDescriptorProto, FieldDescriptorProto)> {
    if f.r#type() != Type::Message {
        return None;
    }
    index.get(f.type_name()).map_entry.clone()
}

fn map_kind(index: &Index, path: &[String], f: &FieldDescriptorProto) -> String {
    // Map entries are proto3-like for packing purposes; only the kind matters.
    let ctx = FileCtx { proto2: false };
    field_gen(index, &ctx, "", path, f).kind
}

fn gen_accessor(
    index: &Index,
    out: &mut String,
    g: &FieldGen,
    f: &FieldDescriptorProto,
    path: &[String],
    _ctx: &FileCtx,
) {
    let id = &g.ident;
    let doc = format!("    /// Field `{}` ({}).", f.name(), g.num);
    if f.label() == Label::Repeated {
        let ty = storage_type(index, g, f, path);
        let _ = writeln!(
            out,
            "{doc}\n    #[inline]\n    pub fn {id}(&self) -> &{ty} {{\n        &self.{id}\n    }}"
        );
        return;
    }
    if g.lazy {
        let _ = writeln!(
            out,
            "{doc} Decoded from the buffer on each call.\n    #[inline]\n    pub fn {id}(&self) -> Option<{}> {{\n        self.{id}.get()\n    }}",
            g.value
        );
        return;
    }
    if !g.scalar {
        let _ = writeln!(out, "{doc}\n    #[inline]\n    pub fn {id}(&self) -> Option<&{}> {{\n        self.{id}.as_ref()\n    }}", g.value);
        return;
    }
    if g.explicit {
        let default = default_expr(index, path, f).unwrap_or_else(|| "Default::default()".into());
        let _ = writeln!(
            out,
            "{doc}\n    #[inline]\n    pub fn {id}(&self) -> {} {{\n        match self.{id} {{\n            Some(v) => v,\n            None => {default},\n        }}\n    }}\n    /// Whether field `{}` was present.\n    #[inline]\n    pub fn {}(&self) -> bool {{\n        self.{id}.is_some()\n    }}",
            g.value,
            f.name(),
            has_ident(id)
        );
        return;
    }
    let _ = writeln!(
        out,
        "{doc}\n    #[inline]\n    pub fn {id}(&self) -> {} {{\n        self.{id}\n    }}",
        g.value
    );
}

#[allow(clippy::too_many_arguments)]
fn gen_merge_arm(
    index: &Index,
    out: &mut String,
    g: &FieldGen,
    f: &FieldDescriptorProto,
    modname: &str,
    oneofs: &[(String, Vec<&FieldDescriptorProto>)],
    in_oneof: bool,
) {
    let n = g.num;
    let id = &g.ident;
    let k = &g.kind;
    let w = g.wire;
    if f.label() == Label::Repeated {
        if map_entry(index, f).is_some() {
            let _ = writeln!(out, "                ({n}, WireType::Len) => self.{id}.read(&mut r, start, {n}, depth)?,");
        } else {
            let _ = writeln!(
                out,
                "                ({n}, w) if giong_proto::Repeated::<{k}>::accepts(w) => self.{id}.read(&mut r, start, {n}, w, depth)?,"
            );
        }
        return;
    }
    if in_oneof {
        let oi = f.oneof_index.unwrap() as usize;
        let oname = &oneofs[oi].0;
        let oid = ident(oname);
        let en = format!("{modname}::{}View", camel(oname));
        let var = camel(f.name());
        if g.lazy {
            let read = if g.group {
                format!("l.read_group(&mut r, {n}, depth)?")
            } else {
                "l.read(&mut r, depth)?".into()
            };
            let _ = writeln!(
                out,
                "                ({n}, WireType::{w}) => {{\n                    if !matches!(self.{oid}, Some({en}::{var}(_))) {{\n                        self.{oid} = Some({en}::{var}(Default::default()));\n                    }}\n                    if let Some({en}::{var}(l)) = &mut self.{oid} {{\n                        {read};\n                    }}\n                }}"
            );
        } else if !g.scalar {
            let _ = writeln!(
                out,
                "                ({n}, WireType::{w}) => {{\n                    if !matches!(self.{oid}, Some({en}::{var}(_))) {{\n                        self.{oid} = Some({en}::{var}(Default::default()));\n                    }}\n                    if let Some({en}::{var}(m)) = &mut self.{oid} {{\n                        <{k}>::read_into(m, &mut r, {n}, depth)?;\n                    }}\n                }}"
            );
        } else if g.closed_enum.is_some() {
            let _ = writeln!(
                out,
                "                ({n}, WireType::{w}) => {{\n                    let v = <{k}>::read(&mut r, {n}, depth)?;\n                    if <{k}>::keep(&v) {{\n                        self.{oid} = Some({en}::{var}(v));\n                    }} else {{\n                        self.__giong_unknown = true;\n                    }}\n                }}"
            );
        } else {
            let _ = writeln!(out, "                ({n}, WireType::{w}) => self.{oid} = Some({en}::{var}(<{k}>::read(&mut r, {n}, depth)?)),");
        }
        return;
    }
    if g.lazy {
        let read = if g.group {
            format!("self.{id}.read_group(&mut r, {n}, depth)?")
        } else {
            format!("self.{id}.read(&mut r, depth)?")
        };
        let _ = writeln!(out, "                ({n}, WireType::{w}) => {read},");
        return;
    }
    if !g.scalar {
        let _ = writeln!(out, "                ({n}, WireType::{w}) => <{k}>::read_into(self.{id}.get_or_insert_with(Default::default), &mut r, {n}, depth)?,");
        return;
    }
    let store = if g.explicit { "Some(v)" } else { "v" };
    if g.closed_enum.is_some() {
        let _ = writeln!(
            out,
            "                ({n}, WireType::{w}) => {{\n                    let v = <{k}>::read(&mut r, {n}, depth)?;\n                    if <{k}>::keep(&v) {{\n                        self.{id} = {store};\n                    }} else {{\n                        self.__giong_unknown = true;\n                    }}\n                }}"
        );
    } else {
        let _ = writeln!(out, "                ({n}, WireType::{w}) => {{\n                    let v = <{k}>::read(&mut r, {n}, depth)?;\n                    self.{id} = {store};\n                }}");
    }
}

fn gen_encode_field(
    out: &mut String,
    g: &FieldGen,
    f: &FieldDescriptorProto,
    _ctx: &FileCtx,
    len: bool,
) {
    let n = g.num;
    let id = &g.ident;
    let k = &g.kind;
    if f.label() == Label::Repeated {
        // Maps and repeated fields carry their own encoders.
        if len {
            let _ = writeln!(
                out,
                "        n += self.{id}.encoded_len({});",
                if g.packed { "true" } else { "false" }
            );
        } else {
            let _ = writeln!(
                out,
                "        self.{id}.encode_raw({}, buf);",
                if g.packed { "true" } else { "false" }
            );
        }
        return;
    }
    let (cond, val) = if g.lazy {
        (
            format!("if let Some(v) = self.{id}.get()"),
            "&v".to_string(),
        )
    } else if !g.scalar || g.explicit {
        (format!("if let Some(v) = &self.{id}"), "v".to_string())
    } else if g.string_like {
        (format!("if !self.{id}.is_empty()"), format!("&self.{id}"))
    } else if g.float {
        (
            format!("if self.{id}.to_bits() != 0"),
            format!("&self.{id}"),
        )
    } else if g.value.starts_with("giong_proto::Enum") {
        (format!("if self.{id}.raw() != 0"), format!("&self.{id}"))
    } else if g.value == "bool" {
        (format!("if self.{id}"), format!("&self.{id}"))
    } else {
        (format!("if self.{id} != 0"), format!("&self.{id}"))
    };
    if len {
        let _ = writeln!(
            out,
            "        {cond} {{\n            n += <{k}>::field_len({n}, {val});\n        }}"
        );
    } else {
        let _ = writeln!(
            out,
            "        {cond} {{\n            <{k}>::put_field({n}, {val}, buf);\n        }}"
        );
    }
}

#[allow(clippy::too_many_arguments)]
fn gen_oneof_encode(
    index: &Index,
    ctx: &FileCtx,
    full: &str,
    path: &[String],
    out: &mut String,
    modname: &str,
    oneof: &(String, Vec<&FieldDescriptorProto>),
    len: bool,
) {
    let (oname, members) = oneof;
    let oid = ident(oname);
    let en = format!("{modname}::{}View", camel(oname));
    let _ = writeln!(out, "        match &self.{oid} {{");
    for f in members {
        let g = field_gen(index, ctx, full, path, f);
        let var = camel(f.name());
        let k = &g.kind;
        let n = g.num;
        let (bind, val) = if g.lazy { ("l", "&v") } else { ("v", "v") };
        let pre = if g.lazy {
            "if let Some(v) = l.get() { "
        } else {
            "{ "
        };
        if len {
            let _ = writeln!(out, "            Some({en}::{var}({bind})) => {pre}n += <{k}>::field_len({n}, {val}); }}");
        } else {
            let _ = writeln!(out, "            Some({en}::{var}({bind})) => {pre}<{k}>::put_field({n}, {val}, buf); }}");
        }
    }
    out.push_str("            _ => {}\n        }\n");
}

fn gen_service(
    index: &Index,
    out: &mut String,
    path: &[String],
    package: &str,
    s: &prost_types::ServiceDescriptorProto,
    with_code: bool,
) {
    let full = if package.is_empty() {
        s.name().to_string()
    } else {
        format!("{package}.{}", s.name())
    };
    let modname = module_name(s.name());
    let mut sub_path = path.to_vec();
    sub_path.push(modname.clone());
    let _ = writeln!(
        out,
        "/// The gRPC service `{full}`: its methods' paths and kinds{}.\npub mod {modname} {{",
        if with_code {
            ", the trait a server implements, the server that routes calls to it, and the client"
        } else {
            ""
        },
    );
    let _ = writeln!(
        out,
        "    /// The service's full name.\n    pub const NAME: &str = \"{full}\";"
    );
    for m in &s.method {
        let kind = match (m.client_streaming(), m.server_streaming()) {
            (false, false) => "Unary",
            (true, false) => "ClientStreaming",
            (false, true) => "ServerStreaming",
            (true, true) => "Bidi",
        };
        let _ = writeln!(
            out,
            "    /// `{}`: `{}` to `{}`.\n    pub const {}: giong_proto::service::Method = giong_proto::service::Method {{\n        path: \"/{full}/{}\",\n        kind: giong_proto::service::MethodKind::{kind},\n        input: \"{}\",\n        output: \"{}\",\n    }};",
            m.name(),
            m.input_type().trim_start_matches('.'),
            m.output_type().trim_start_matches('.'),
            snake(m.name()).to_ascii_uppercase(),
            m.name(),
            m.input_type().trim_start_matches('.'),
            m.output_type().trim_start_matches('.'),
        );
    }
    let _ = writeln!(
        out,
        "    /// Every method of the service, in the order of the `.proto`.\n    pub const METHODS: &[giong_proto::service::Method] = &[{}];",
        s.method
            .iter()
            .map(|m| snake(m.name()).to_ascii_uppercase())
            .collect::<Vec<_>>()
            .join(", ")
    );
    if with_code {
        gen_service_code(index, out, &sub_path, &full, s);
    }
    out.push_str("}\n");
}

/// The trait, the server and the client of a service, inside its module.
fn gen_service_code(
    index: &Index,
    out: &mut String,
    sub_path: &[String],
    full: &str,
    s: &prost_types::ServiceDescriptorProto,
) {
    let name = type_name(s.name());
    let ty = |full_name: &str| {
        let info = index.get(full_name);
        format!("{}{}<'static>", rel_path(sub_path, info), info.name)
    };
    const RESULT: &str = "::core::result::Result";
    const FUTURE: &str = "::core::future::Future";

    // The trait.
    let _ = writeln!(
        out,
        "\n    /// The service `{full}`, as a server implements it: one method for each\n    /// rpc. Returning `Err(status)` ends the call with that status. Each call\n    /// has its own future, on the worker its connection lives on.\n    pub trait {name}: 'static {{"
    );
    for m in &s.method {
        let id = ident(&snake(m.name()));
        let (req, resp) = (ty(m.input_type()), ty(m.output_type()));
        let (params, output, what) = match (m.client_streaming(), m.server_streaming()) {
            (false, false) => (
                format!("request: giong_grpc::Request<{req}>"),
                format!("giong_grpc::Reply<{resp}>"),
                "one request, one response",
            ),
            (true, false) => (
                format!("requests: giong_grpc::RequestStream<{req}>"),
                format!("giong_grpc::Reply<{resp}>"),
                "a stream of requests, one response",
            ),
            (false, true) => (
                format!("request: giong_grpc::Request<{req}>, responses: giong_grpc::Sink<{resp}>"),
                "()".to_string(),
                "one request, a stream of responses sent through `responses`",
            ),
            (true, true) => (
                format!(
                    "requests: giong_grpc::RequestStream<{req}>, responses: giong_grpc::Sink<{resp}>"
                ),
                "()".to_string(),
                "a stream of requests, a stream of responses sent through `responses`",
            ),
        };
        let _ = writeln!(
            out,
            "        /// `{}`: {what}.\n        fn {id}(\n            &self,\n            {params},\n        ) -> impl {FUTURE}<Output = {RESULT}<{output}, giong_grpc::Status>>;",
            m.name()
        );
    }
    out.push_str("    }\n");

    // The server.
    let _ = writeln!(
        out,
        "\n    /// Routes the calls of `{full}` to an implementation of [`{name}`]: what\n    /// `giong_grpc::Server::add` takes.\n    pub struct {name}Server<T> {{\n        inner: ::std::rc::Rc<T>,\n    }}\n\n    impl<T: {name}> {name}Server<T> {{\n        /// A server for `service`.\n        pub fn new(service: T) -> Self {{\n            Self {{\n                inner: ::std::rc::Rc::new(service),\n            }}\n        }}\n    }}\n\n    impl<T: {name}> giong_grpc::Service for {name}Server<T> {{\n        fn name(&self) -> &'static str {{\n            NAME\n        }}\n\n        fn call(\n            &self,\n            method: &str,\n            call: giong_grpc::server::Call,\n        ) -> giong_grpc::server::CallFuture {{\n            let inner = self.inner.clone();\n            match method {{"
    );
    for m in &s.method {
        let id = ident(&snake(m.name()));
        let (req, resp) = (ty(m.input_type()), ty(m.output_type()));
        let arm = match (m.client_streaming(), m.server_streaming()) {
            (false, false) => format!(
                "call.unary::<{req}, {resp}, _, _>(move |request| async move {{ inner.{id}(request).await }})"
            ),
            (true, false) => format!(
                "call.client_streaming::<{req}, {resp}, _, _>(move |requests| async move {{ inner.{id}(requests).await }})"
            ),
            (false, true) => format!(
                "call.server_streaming::<{req}, {resp}, _, _>(move |request, responses| async move {{ inner.{id}(request, responses).await }})"
            ),
            (true, true) => format!(
                "call.bidi::<{req}, {resp}, _, _>(move |requests, responses| async move {{ inner.{id}(requests, responses).await }})"
            ),
        };
        let _ = writeln!(out, "                \"{}\" => {arm},", m.name());
    }
    out.push_str("                _ => call.unimplemented(),\n            }\n        }\n    }\n");

    // The client.
    let _ = writeln!(
        out,
        "\n    /// Calls `{full}` on a channel. Cloning it is cheap: the clones share\n    /// the channel.\n    #[derive(Clone, Debug)]\n    pub struct {name}Client {{\n        channel: giong_grpc::Channel,\n    }}\n\n    impl {name}Client {{\n        /// A client that makes its calls on `channel`.\n        pub fn new(channel: giong_grpc::Channel) -> Self {{\n            Self {{ channel }}\n        }}"
    );
    for m in &s.method {
        let id = ident(&snake(m.name()));
        let konst = snake(m.name()).to_ascii_uppercase();
        let (req, resp) = (ty(m.input_type()), ty(m.output_type()));
        match (m.client_streaming(), m.server_streaming()) {
            (false, false) => {
                let _ = writeln!(
                    out,
                    "\n        /// Calls `{}`: sends `message` and returns the response.\n        ///\n        /// Cancel safe: dropping the future cancels the call.\n        pub async fn {id}(\n            &self,\n            message: &impl giong_proto::MessageOf<Type = {req}>,\n            options: giong_grpc::CallOptions,\n        ) -> {RESULT}<giong_grpc::Reply<{resp}>, giong_grpc::Status> {{\n            self.channel.unary({konst}.path, message, options).await\n        }}",
                    m.name()
                );
            }
            (false, true) => {
                let _ = writeln!(
                    out,
                    "\n        /// Calls `{}`: sends `message` and returns the stream of responses\n        /// once the server has started to answer. Dropping the stream cancels\n        /// the call.\n        ///\n        /// Cancel safe: dropping the future cancels the call.\n        pub async fn {id}(\n            &self,\n            message: &impl giong_proto::MessageOf<Type = {req}>,\n            options: giong_grpc::CallOptions,\n        ) -> {RESULT}<giong_grpc::ResponseStream<{resp}>, giong_grpc::Status> {{\n            self.channel.server_streaming({konst}.path, message, options).await\n        }}",
                    m.name()
                );
            }
            (true, false) => {
                let _ = writeln!(
                    out,
                    "\n        /// Starts `{}`: send the requests on the value returned, then\n        /// `finish` it for the response.\n        pub fn {id}(\n            &self,\n            options: giong_grpc::CallOptions,\n        ) -> giong_grpc::ClientStreaming<{req}, {resp}> {{\n            self.channel.client_streaming({konst}.path, options)\n        }}",
                    m.name()
                );
            }
            (true, true) => {
                let _ = writeln!(
                    out,
                    "\n        /// Starts `{}`: returns the sink for the requests and the stream of\n        /// responses. Dropping the stream cancels the call.\n        pub fn {id}(\n            &self,\n            options: giong_grpc::CallOptions,\n        ) -> (\n            giong_grpc::RequestSink<{req}>,\n            giong_grpc::ResponseStream<{resp}>,\n        ) {{\n            self.channel.bidi({konst}.path, options)\n        }}",
                    m.name()
                );
            }
        }
    }
    out.push_str("    }\n");
}

/// The Rust type of one value of `f` in a message struct (the writer):
/// numbers, `&'a str`, `&'a [u8]`, `Enum<E>`, or the nested message struct.
fn writer_value(index: &Index, from: &[String], f: &FieldDescriptorProto, g: &FieldGen) -> String {
    match f.r#type() {
        Type::Message | Type::Group => {
            let info = index.get(f.type_name());
            format!("{}{}<'a>", rel_path(from, info), info.name)
        }
        _ => g.value.clone(),
    }
}

fn gen_writer(
    index: &Index,
    ctx: &FileCtx,
    parent: &mut Module,
    path: &[String],
    full: &str,
    msg: &DescriptorProto,
    oneofs: &[(String, Vec<&FieldDescriptorProto>)],
) {
    let name = type_name(msg.name());
    let modname = module_name(msg.name());
    let mut sub_path = path.to_vec();
    sub_path.push(modname.clone());
    let in_oneof = |f: &FieldDescriptorProto| f.oneof_index.is_some() && !f.proto3_optional();
    let mut fields: Vec<&FieldDescriptorProto> = msg.field.iter().collect();
    fields.sort_by_key(|f| f.number());

    let mut out = String::new();
    let _ = writeln!(
        out,
        "/// The message `{}`, to build and encode. Fields borrow; nothing is copied until `encode`.\n#[derive(Clone, Debug, Default, PartialEq)]\npub struct {name}<'a> {{",
        &full[1..]
    );
    for f in &fields {
        if in_oneof(f) {
            continue;
        }
        let g = field_gen(index, ctx, full, path, f);
        let v = writer_value(index, path, f, &g);
        let ty = if f.label() == Label::Repeated {
            match map_entry(index, f) {
                Some((k, val)) => {
                    let kg = field_gen(index, &FileCtx { proto2: false }, "", path, &k);
                    let vg = field_gen(index, &FileCtx { proto2: false }, "", path, &val);
                    format!(
                        "&'a [({}, {})]",
                        writer_value(index, path, &k, &kg),
                        writer_value(index, path, &val, &vg)
                    )
                }
                None => format!("&'a [{v}]"),
            }
        } else if !g.scalar {
            format!("Option<&'a {v}>")
        } else if g.explicit {
            format!("Option<{v}>")
        } else {
            v
        };
        let _ = writeln!(
            out,
            "    /// Field `{}` ({}).\n    pub {}: {ty},",
            f.name(),
            g.num,
            g.ident
        );
    }
    for (oname, members) in oneofs {
        if !members.is_empty() {
            let _ = writeln!(
                out,
                "    /// The oneof `{oname}`.\n    pub {}: Option<{modname}::{}<'a>>,",
                ident(oname),
                camel(oname)
            );
        }
    }
    out.push_str("    #[doc(hidden)]\n    pub __phantom: std::marker::PhantomData<&'a ()>,\n}\n");

    // Encode.
    for len in [true, false] {
        if len {
            let _ = writeln!(
                out,
                "impl<'a> giong_proto::Encode for {name}<'a> {{\n    fn encoded_len(&self) -> usize {{\n        use giong_proto::view::Kind;\n        let mut n = 0;"
            );
        } else {
            out.push_str("    fn encode_raw(&self, buf: &mut Vec<u8>) {\n        use giong_proto::view::Kind;\n");
        }
        let mut done = std::collections::HashSet::new();
        for f in &fields {
            if in_oneof(f) {
                let oi = f.oneof_index.unwrap() as usize;
                if done.insert(oi) {
                    let (oname, members) = &oneofs[oi];
                    let _ = writeln!(out, "        match &self.{} {{", ident(oname));
                    for m in members {
                        let g = field_gen(index, ctx, full, path, m);
                        let var = camel(m.name());
                        let en = format!("{modname}::{}", camel(oname));
                        let body = writer_single(&g, "v", len);
                        let _ = writeln!(out, "            Some({en}::{var}(v)) => {{ {body} }}");
                    }
                    out.push_str("            _ => {}\n        }\n");
                }
                continue;
            }
            let g = field_gen(index, ctx, full, path, f);
            let id = &g.ident;
            let n = g.num;
            if f.label() == Label::Repeated {
                if let Some((k, val)) = map_entry(index, f) {
                    let kg = field_gen(index, &FileCtx { proto2: false }, "", path, &k);
                    let vg = field_gen(index, &FileCtx { proto2: false }, "", path, &val);
                    let vlen = if vg.scalar {
                        format!("<{}>::field_len(2, v)", vg.kind)
                    } else {
                        "giong_proto::view::message_len(2, v)".to_string()
                    };
                    let vput = if vg.scalar {
                        format!("<{}>::put_field(2, v, buf)", vg.kind)
                    } else {
                        "giong_proto::view::put_message(2, v, buf)".to_string()
                    };
                    let entry = format!("<{}>::field_len(1, k) + {vlen}", kg.kind);
                    if len {
                        let _ = writeln!(
                            out,
                            "        for (k, v) in self.{id} {{\n            n += giong_proto::wire::tag_len({n}) + giong_proto::wire::len_delimited({entry});\n        }}"
                        );
                    } else {
                        let _ = writeln!(
                            out,
                            "        for (k, v) in self.{id} {{\n            giong_proto::wire::put_tag(buf, {n}, giong_proto::wire::WireType::Len);\n            giong_proto::wire::put_varint(buf, ({entry}) as u64);\n            <{}>::put_field(1, k, buf);\n            {vput};\n        }}",
                            kg.kind
                        );
                    }
                } else if g.scalar {
                    let k = &g.kind;
                    if len {
                        let _ = writeln!(out, "        n += giong_proto::view::repeated_len::<{k}>({n}, self.{id}, {});", g.packed);
                    } else {
                        let _ = writeln!(out, "        giong_proto::view::put_repeated::<{k}>({n}, self.{id}, {}, buf);", g.packed);
                    }
                } else {
                    let body = writer_single(&g, "v", len);
                    let _ = writeln!(
                        out,
                        "        for v in self.{id} {{\n            {body}\n        }}"
                    );
                }
                continue;
            }
            if !g.scalar {
                let body = writer_single(&g, "v", len);
                let _ = writeln!(
                    out,
                    "        if let Some(v) = self.{id} {{\n            {body}\n        }}"
                );
            } else if g.explicit {
                let body = writer_single(&g, "v", len);
                let _ = writeln!(
                    out,
                    "        if let Some(v) = &self.{id} {{\n            {body}\n        }}"
                );
            } else {
                let cond = if g.string_like {
                    format!("!self.{id}.is_empty()")
                } else if g.float {
                    format!("self.{id}.to_bits() != 0")
                } else if g.value.starts_with("giong_proto::Enum") {
                    format!("self.{id}.raw() != 0")
                } else if g.value == "bool" {
                    format!("self.{id}")
                } else {
                    format!("self.{id} != 0")
                };
                let body = writer_single(&g, &format!("&self.{id}"), len);
                let _ = writeln!(out, "        if {cond} {{\n            {body}\n        }}");
            }
        }
        if len {
            out.push_str("        n\n    }\n");
        } else {
            out.push_str("    }\n}\n");
        }
    }

    // Oneof enums for the writer.
    let mut sub_code = String::new();
    for (oname, members) in oneofs {
        if members.is_empty() {
            continue;
        }
        let _ = writeln!(sub_code, "/// The oneof `{oname}` of `{}`, to build.\n#[derive(Clone, Debug, PartialEq)]\npub enum {}<'a> {{", &full[1..], camel(oname));
        for f in members {
            let g = field_gen(index, ctx, full, &sub_path, f);
            let v = writer_value(index, &sub_path, f, &g);
            let ty = if g.scalar { v } else { format!("&'a {v}") };
            let _ = writeln!(
                sub_code,
                "    /// Field `{}` ({}).\n    {}({ty}),",
                f.name(),
                g.num,
                camel(f.name())
            );
        }
        sub_code
            .push_str("    #[doc(hidden)]\n    __Phantom(std::marker::PhantomData<&'a ()>),\n}\n");
    }
    parent
        .children
        .entry(modname)
        .or_default()
        .code
        .push_str(&sub_code);
    parent.code.push_str(&out);
}

/// Code that adds one value's length to `n`, or appends it to `buf`.
fn writer_single(g: &FieldGen, v: &str, len: bool) -> String {
    let n = g.num;
    match (g.scalar, g.group, len) {
        (true, _, true) => format!("n += <{}>::field_len({n}, {v});", g.kind),
        (true, _, false) => format!("<{}>::put_field({n}, {v}, buf);", g.kind),
        (false, true, true) => format!("n += giong_proto::view::group_len({n}, {v});"),
        (false, true, false) => format!("giong_proto::view::put_group({n}, {v}, buf);"),
        (false, false, true) => format!("n += giong_proto::view::message_len({n}, {v});"),
        (false, false, false) => format!("giong_proto::view::put_message({n}, {v}, buf);"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names() {
        assert_eq!(snake("TestAllTypesProto3"), "test_all_types_proto3");
        assert_eq!(snake("HTTPRequest"), "http_request");
        assert_eq!(camel("oneof_field"), "OneofField");
        assert_eq!(ident("type"), "r#type");
        assert_eq!(ident("self"), "self_");
        assert_eq!(unescape_c(r"a\001\x7f\n\\"), b"a\x01\x7f\n\\");
    }
}
