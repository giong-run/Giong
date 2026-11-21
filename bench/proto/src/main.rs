//! giong-proto against prost on the same bytes.
//!
//! For each message shape: decode only, decode and read every field (a
//! fair comparison, since a giong-proto view reads repeated fields lazily),
//! and encode an already decoded message into a reused buffer. Prints one
//! CSV line per round: `case,impl,op,round,ns_per_op,bytes`.
//!
//! `ROUNDS` (default 10) and `ROUND_MS` (default 300) set the length.

use std::hint::black_box;
use std::time::{Duration, Instant};

use giong_proto::Encode;
use prost::Message as _;

mod pb {
    include!(concat!(env!("OUT_DIR"), "/giong_proto.rs"));
}
#[allow(dead_code)]
mod pp {
    include!(concat!(env!("OUT_DIR"), "/bench.rs"));
}

use pb::bench as g;

/// Counts heap allocations, to report how many each decode makes.
struct Counting;

static ALLOCS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

unsafe impl std::alloc::GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: std::alloc::Layout) -> *mut u8 {
        ALLOCS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        std::alloc::System.alloc(layout)
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: std::alloc::Layout) {
        std::alloc::System.dealloc(ptr, layout)
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: std::alloc::Layout, new: usize) -> *mut u8 {
        ALLOCS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        std::alloc::System.realloc(ptr, layout, new)
    }
}

#[global_allocator]
static GLOBAL: Counting = Counting;

/// Heap allocations one call of `f` makes.
fn allocs(mut f: impl FnMut() -> u64) -> u64 {
    let before = ALLOCS.load(std::sync::atomic::Ordering::Relaxed);
    black_box(f());
    ALLOCS.load(std::sync::atomic::Ordering::Relaxed) - before
}

fn rounds() -> usize {
    std::env::var("ROUNDS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(10)
}

fn round_time() -> Duration {
    Duration::from_millis(
        std::env::var("ROUND_MS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(300),
    )
}

/// Runs `f` for ROUNDS rounds and prints ns per call for each.
fn measure(case: &str, imp: &str, op: &str, bytes: usize, mut f: impl FnMut() -> u64) {
    // Calibrate: how many calls fill about 10 ms.
    let mut batch = 1u64;
    loop {
        let t = Instant::now();
        for _ in 0..batch {
            black_box(f());
        }
        if t.elapsed() > Duration::from_millis(10) {
            break;
        }
        batch *= 2;
    }
    // Warm up.
    let t = Instant::now();
    while t.elapsed() < round_time() {
        for _ in 0..batch {
            black_box(f());
        }
    }
    for round in 0..rounds() {
        let mut calls = 0u64;
        let t = Instant::now();
        while t.elapsed() < round_time() {
            for _ in 0..batch {
                black_box(f());
            }
            calls += batch;
        }
        let ns = t.elapsed().as_nanos() as f64 / calls as f64;
        println!("{case},{imp},{op},{round},{ns:.2},{bytes}");
    }
}

// ---- inputs, built with prost ----

fn small() -> pp::Small {
    pp::Small {
        id: 4242,
        timestamp: 1_794_757_564_111,
        flags: 0x8001,
        urgent: true,
        score: 0.875,
        trace_id: 0xdead_beef_cafe_f00d,
    }
}

fn user(i: u64) -> pp::User {
    pp::User {
        id: 1_000_000 + i,
        name: format!("Nguyễn Văn Người Dùng {i}"),
        email: format!("user{i}@example.com"),
        country: "VN".into(),
        tags: vec![
            "premium".into(),
            "beta-tester".into(),
            "early-adopter".into(),
            "newsletter".into(),
        ],
        avatar_hash: vec![0xab; 32],
        attributes: [("plan", "pro"), ("locale", "vi-VN"), ("theme", "dark")]
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect(),
    }
}

fn item_list(n: u64) -> pp::ItemList {
    pp::ItemList {
        items: (0..n)
            .map(|i| pp::Item {
                id: 10_000 + i,
                name: format!("Item number {i} with a reasonably long product name"),
                price_cents: 1999 + i as i64 * 7,
                labels: vec!["in-stock".into(), "free-shipping".into()],
                stock_by_store: vec![3, 0, 12, 7, 1],
            })
            .collect(),
        next_page_token: "eyJvZmZzZXQiOjEwMH0".into(),
    }
}

fn blob() -> pp::Blob {
    pp::Blob {
        name: "upload.bin".into(),
        data: (0..64 * 1024).map(|i| (i * 31 % 251) as u8).collect(),
    }
}

fn descriptors() -> Vec<u8> {
    // The descriptors of descriptor.proto and the benchmark's own file:
    // the workload upb and protobuf's own benchmarks use.
    let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/proto");
    let fds = protox::compile([format!("{dir}/bench.proto")], [dir]).unwrap();
    fds.encode_to_vec()
}

// ---- reading every field ----

fn touch_small_p(m: &pp::Small) -> u64 {
    m.id as u64
        ^ m.timestamp as u64
        ^ m.flags as u64
        ^ m.urgent as u64
        ^ m.score.to_bits()
        ^ m.trace_id
}
fn touch_small_g(m: &g::SmallView<'_>) -> u64 {
    m.id() as u64
        ^ m.timestamp() as u64
        ^ m.flags() as u64
        ^ m.urgent() as u64
        ^ m.score().to_bits()
        ^ m.trace_id()
}

fn touch_user_p(m: &pp::User) -> u64 {
    let mut s = m.id
        + m.name.len() as u64
        + m.email.len() as u64
        + m.country.len() as u64
        + m.avatar_hash.len() as u64;
    for t in &m.tags {
        s += t.len() as u64;
    }
    for (k, v) in &m.attributes {
        s += (k.len() + v.len()) as u64;
    }
    s
}
fn touch_user_g(m: &g::UserView<'_>) -> u64 {
    let mut s = m.id()
        + m.name().len() as u64
        + m.email().len() as u64
        + m.country().len() as u64
        + m.avatar_hash().len() as u64;
    for t in m.tags() {
        s += t.len() as u64;
    }
    for (k, v) in m.attributes().iter() {
        s += (k.len() + v.len()) as u64;
    }
    s
}

fn touch_list_p(m: &pp::ItemList) -> u64 {
    let mut s = m.next_page_token.len() as u64;
    for it in &m.items {
        s += it.id + it.name.len() as u64 + it.price_cents as u64;
        for l in &it.labels {
            s += l.len() as u64;
        }
        for x in &it.stock_by_store {
            s += *x as u64;
        }
    }
    s
}
fn touch_list_g(m: &g::ItemListView<'_>) -> u64 {
    let mut s = m.next_page_token().len() as u64;
    for it in m.items() {
        s += it.id() + it.name().len() as u64 + it.price_cents() as u64;
        for l in it.labels() {
            s += l.len() as u64;
        }
        for x in it.stock_by_store() {
            s += x as u64;
        }
    }
    s
}

fn touch_blob_p(m: &pp::Blob) -> u64 {
    m.name.len() as u64 + m.data.len() as u64 + m.data[m.data.len() / 2] as u64
}
fn touch_blob_g(m: &g::BlobView<'_>) -> u64 {
    m.name().len() as u64 + m.data().len() as u64 + m.data()[m.data().len() / 2] as u64
}

fn touch_desc_p(m: &prost_types::FileDescriptorSet) -> u64 {
    fn msg(d: &prost_types::DescriptorProto) -> u64 {
        let mut s = d.name().len() as u64;
        for f in &d.field {
            s += f.name().len() as u64 + f.number() as u64 + f.type_name().len() as u64;
        }
        for n in &d.nested_type {
            s += msg(n);
        }
        s
    }
    let mut s = 0;
    for f in &m.file {
        s += f.name().len() as u64;
        for d in &f.message_type {
            s += msg(d);
        }
    }
    s
}
fn touch_desc_g(m: &pb::google::protobuf::FileDescriptorSetView<'_>) -> u64 {
    fn msg(d: &pb::google::protobuf::DescriptorProtoView<'_>) -> u64 {
        let mut s = d.name().len() as u64;
        for f in d.field() {
            s += f.name().len() as u64 + f.number() as u64 + f.type_name().len() as u64;
        }
        for n in d.nested_type() {
            s += msg(&n);
        }
        s
    }
    let mut s = 0;
    for f in m.file() {
        s += f.name().len() as u64;
        for d in f.message_type() {
            s += msg(&d);
        }
    }
    s
}

macro_rules! case {
    ($name:expr, $bytes:expr, $prost:ty, $view:ty, $tp:ident, $tg:ident) => {{
        let buf: Vec<u8> = $bytes;
        let n = buf.len();
        // Both must read the same values.
        let p = <$prost>::decode(&buf[..]).unwrap();
        let v = <$view>::decode(&buf).unwrap();
        assert_eq!(
            $tp(&p),
            $tg(&v),
            "{}: prost and giong-proto read different values",
            $name
        );
        eprintln!("{}: {} bytes", $name, n);
        // Allocations per decode-and-read, as one row with round 0.
        println!(
            "{},prost,allocs,0,{},{n}",
            $name,
            allocs(|| $tp(&<$prost>::decode(&buf[..]).unwrap()))
        );
        println!(
            "{},giong,allocs,0,{},{n}",
            $name,
            allocs(|| $tg(&<$view>::decode(&buf).unwrap()))
        );
        measure($name, "prost", "decode", n, || {
            let m = <$prost>::decode(black_box(&buf[..])).unwrap();
            black_box(&m);
            0
        });
        measure($name, "giong", "decode", n, || {
            let m = <$view>::decode(black_box(&buf)).unwrap();
            black_box(&m);
            0
        });
        measure($name, "prost", "decode+read", n, || {
            $tp(&<$prost>::decode(black_box(&buf[..])).unwrap())
        });
        measure($name, "giong", "decode+read", n, || {
            $tg(&<$view>::decode(black_box(&buf)).unwrap())
        });
        // Forwarding a received message: prost re-encodes its struct, a
        // giong-proto view copies the bytes it was decoded from.
        let mut out = Vec::with_capacity(n * 2);
        measure($name, "prost", "forward", n, || {
            out.clear();
            black_box(&p).encode(&mut out).unwrap();
            out.len() as u64
        });
        measure($name, "giong", "forward", n, || {
            out.clear();
            black_box(&v).encode(&mut out).unwrap();
            out.len() as u64
        });
    }};
}

/// Encoding a message built in code: prost's struct against giong-proto's
/// message struct (borrowed fields). Both encodings must read back as the
/// same message.
fn encode_case<P: prost::Message + PartialEq + Default + std::fmt::Debug, W: Encode>(
    name: &str,
    p: &P,
    w: &W,
) {
    let pb = p.encode_to_vec();
    let wb = w.encode_to_vec().unwrap();
    assert_eq!(w.encoded_len(), wb.len(), "{name}: encoded_len");
    assert_eq!(
        P::decode(&wb[..]).unwrap(),
        *p,
        "{name}: giong-proto's encoding reads back differently"
    );
    let n = pb.len();
    let mut out = Vec::with_capacity(n * 2);
    measure(name, "prost", "encode", n, || {
        out.clear();
        black_box(p).encode(&mut out).unwrap();
        out.len() as u64
    });
    measure(name, "giong", "encode", n, || {
        out.clear();
        black_box(w).encode(&mut out).unwrap();
        out.len() as u64
    });
}

fn main() {
    println!("case,impl,op,round,ns_per_op,bytes");
    case!(
        "small",
        small().encode_to_vec(),
        pp::Small,
        g::SmallView,
        touch_small_p,
        touch_small_g
    );
    case!(
        "user",
        user(7).encode_to_vec(),
        pp::User,
        g::UserView,
        touch_user_p,
        touch_user_g
    );
    case!(
        "item_list_100",
        item_list(100).encode_to_vec(),
        pp::ItemList,
        g::ItemListView,
        touch_list_p,
        touch_list_g
    );
    case!(
        "blob_64k",
        blob().encode_to_vec(),
        pp::Blob,
        g::BlobView,
        touch_blob_p,
        touch_blob_g
    );
    case!(
        "descriptors",
        descriptors(),
        prost_types::FileDescriptorSet,
        pb::google::protobuf::FileDescriptorSetView,
        touch_desc_p,
        touch_desc_g
    );

    // Encoding from message structs.
    let ps = small();
    let ws = g::Small {
        id: ps.id,
        timestamp: ps.timestamp,
        flags: ps.flags,
        urgent: ps.urgent,
        score: ps.score,
        trace_id: ps.trace_id,
        ..Default::default()
    };
    encode_case("small", &ps, &ws);

    let pu = user(7);
    let tags: Vec<&str> = pu.tags.iter().map(|s| s.as_str()).collect();
    // prost keeps maps in a HashMap; compare with the same entries.
    let attrs: Vec<(&str, &str)> = pu
        .attributes
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();
    let wu = g::User {
        id: pu.id,
        name: &pu.name,
        email: &pu.email,
        country: &pu.country,
        tags: &tags,
        avatar_hash: &pu.avatar_hash,
        attributes: &attrs,
        ..Default::default()
    };
    encode_case("user", &pu, &wu);

    let pl = item_list(100);
    let labels: Vec<Vec<&str>> = pl
        .items
        .iter()
        .map(|it| it.labels.iter().map(|s| s.as_str()).collect())
        .collect();
    let items: Vec<g::Item> = pl
        .items
        .iter()
        .zip(&labels)
        .map(|(it, l)| g::Item {
            id: it.id,
            name: &it.name,
            price_cents: it.price_cents,
            labels: l,
            stock_by_store: &it.stock_by_store,
            ..Default::default()
        })
        .collect();
    let wl = g::ItemList {
        items: &items,
        next_page_token: &pl.next_page_token,
        ..Default::default()
    };
    encode_case("item_list_100", &pl, &wl);

    let pbl = blob();
    let wb = g::Blob {
        name: &pbl.name,
        data: &pbl.data,
        ..Default::default()
    };
    encode_case("blob_64k", &pbl, &wb);
}
