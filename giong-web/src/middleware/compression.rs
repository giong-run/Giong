//! [`Compression`]: gzip response bodies for clients that accept it.

use std::cell::RefCell;

#[cfg(feature = "fast-gzip")]
use libdeflater::{CompressionLvl, Compressor};

use miniz_oxide::deflate::core::{
    compress_to_output, create_comp_flags_from_zip_params, CompressorOxide, TDEFLFlush, TDEFLStatus,
};

use super::cors::vary;
use crate::{Error, Middleware, Next, Req, Response};

/// Input compressed between two chances for the worker's other tasks to
/// run: about half a millisecond of work at level 1.
const CHUNK: usize = 64 * 1024;
#[cfg(feature = "fast-gzip")]
const SMALL_BODY_LIMIT: usize = 16 * 1024;

/// Compresses response bodies with gzip when the client accepts it.
///
/// A response is compressed when all of these hold:
///
/// * the request's `Accept-Encoding` accepts `gzip` (q-values are read; `*`
///   counts);
/// * the `Content-Type` is text: `text/*` (but not `text/event-stream`),
///   JSON (`application/json`, `*+json`, `application/x-ndjson`), XML
///   (`application/xml`, `*+xml`, so SVG too), JavaScript or WebAssembly;
/// * the body is between [`min_size`](Self::min_size) and
///   [`max_size`](Self::max_size) bytes;
/// * the response has no `Content-Encoding` or `Content-Range` yet and no
///   `Cache-Control: no-transform`;
/// * gzip actually makes it smaller.
///
/// Every response with a compressible type and size gets `Vary:
/// Accept-Encoding`, compressed or not, so that caches keep the two apart. A
/// strong `ETag` becomes weak (`W/"..."`) on a compressed body, since the
/// bytes differ from the uncompressed ones.
///
/// A `HEAD` request is answered as its `GET` would be, `Content-Encoding`
/// included, as RFC 9110 asks: the body is compressed, and the server sends
/// its length and leaves it out.
///
/// Large bodies are compressed 64 KiB at a time, letting the worker's other
/// connections run in between, so one big response does not hold up the
/// others on its worker.
#[derive(Clone, Debug)]
pub struct Compression {
    level: u8,
    min_size: usize,
    max_size: usize,
}

impl Default for Compression {
    fn default() -> Self {
        Self::new()
    }
}

impl Compression {
    /// gzip at level 1, for bodies from 1 KiB to 16 MiB.
    pub fn new() -> Self {
        Self {
            level: 1,
            min_size: 1024,
            max_size: 16 * 1024 * 1024,
        }
    }

    /// The compression level, from 1 (fastest) to 9 (smallest). Default: 1,
    /// which already shrinks typical JSON to a fifth or less at several
    /// hundred megabytes a second per core.
    ///
    /// # Panics
    ///
    /// Panics if `level` is not in `1..=9` (`GIONG-E1213`).
    #[track_caller]
    pub fn level(mut self, level: u8) -> Self {
        if !(1..=9).contains(&level) {
            super::invalid_config(
                format!("Compression::level({level}): the level is 1 to 9"),
                "use 1 for speed (the default) or up to 9 for size.",
            );
        }
        self.level = level;
        self
    }

    /// Smallest body compressed, in bytes (default: 1024). Below about a
    /// kilobyte the gzip header and the work outweigh the saving.
    pub fn min_size(mut self, bytes: usize) -> Self {
        self.min_size = bytes;
        self
    }

    /// Largest body compressed, in bytes (default: 16 MiB). Larger bodies
    /// are sent as they are.
    pub fn max_size(mut self, bytes: usize) -> Self {
        self.max_size = bytes;
        self
    }

    /// Whether the response is one this middleware would compress for a
    /// client that accepts gzip.
    fn eligible(&self, resp: &Response) -> bool {
        let len = resp.body_bytes().len();
        if resp.is_streaming() || resp.body_is_file() {
            // Its bytes are not known yet, or not in memory: a file is sent
            // as it is, from the page cache.
            return false;
        }
        if matches!(resp.status(), 204 | 206 | 304) || len < self.min_size || len > self.max_size {
            return false;
        }
        if resp.get_header("content-encoding").is_some()
            || resp.get_header("content-range").is_some()
        {
            return false;
        }
        let no_transform = resp.headers().any(|(n, v)| {
            n.eq_ignore_ascii_case("cache-control")
                && crate::header::list_contains(v, "no-transform")
        });
        !no_transform && resp.get_header("content-type").is_some_and(compressible)
    }
}

impl<S: 'static> Middleware<S> for Compression {
    async fn call(&self, req: Req<S>, next: Next<'_, S>) -> Result<Response, Error> {
        let accepts = req.header_str("accept-encoding").is_some_and(accepts_gzip);
        let resp = next.run(req).await;
        if !self.eligible(&resp) {
            return Ok(resp);
        }
        let mut resp = vary(resp, "Accept-Encoding");
        if !accepts {
            return Ok(resp);
        }
        let body = resp.take_body();
        match gzip(body.as_bytes(), self.level).await {
            Some(gz) if gz.len() < body.len() => {
                let resp = weaken_etag(resp);
                Ok(resp.header("Content-Encoding", "gzip").body(gz))
            }
            _ => Ok(resp.body(body)),
        }
    }
}

/// `true` if `Accept-Encoding: value` accepts gzip.
fn accepts_gzip(value: &str) -> bool {
    let (mut gzip, mut any) = (None, None);
    for part in value.split(',') {
        let mut params = part.split(';');
        let coding = params.next().unwrap_or("").trim();
        let q = params
            .filter_map(|p| {
                let (k, v) = p.split_once('=')?;
                k.trim().eq_ignore_ascii_case("q").then(|| v.trim())
            })
            .next()
            .map_or(1.0, |q| q.parse::<f32>().unwrap_or(0.0));
        if coding.eq_ignore_ascii_case("gzip") || coding.eq_ignore_ascii_case("x-gzip") {
            gzip = Some(q);
        } else if coding == "*" {
            any = Some(q);
        }
    }
    gzip.or(any).is_some_and(|q| q > 0.0)
}

/// Media types worth compressing.
fn compressible(content_type: &str) -> bool {
    let mime = content_type
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    if mime == "text/event-stream" {
        return false;
    }
    mime.starts_with("text/")
        || mime.ends_with("+json")
        || mime.ends_with("+xml")
        || matches!(
            mime.as_str(),
            "application/json"
                | "application/x-ndjson"
                | "application/xml"
                | "application/javascript"
                | "application/ecmascript"
                | "application/wasm"
        )
}

/// A strong `ETag` made weak: the compressed bytes are not the ones it
/// names.
fn weaken_etag(resp: Response) -> Response {
    match resp.get_header("etag") {
        Some(tag) if tag.starts_with('"') => {
            let weak = format!("W/{tag}");
            resp.remove_header("etag").header("ETag", &weak)
        }
        _ => resp,
    }
}

thread_local! {
    /// Compressors kept between responses: each holds a few hundred
    /// kilobytes of tables, too much to allocate and clear per response.
    static COMPRESSORS: RefCell<Vec<(u8, Box<CompressorOxide>)>> = const { RefCell::new(Vec::new()) };
}

#[cfg(feature = "fast-gzip")]
thread_local! {
    // No yield or user callback while this worker-local compressor is borrowed.
    static SMALL_GZIP: RefCell<Compressor> = RefCell::new(Compressor::new(
        CompressionLvl::new(1).expect("libdeflate supports level 1")
    ));
}

#[cfg(feature = "fast-gzip")]
fn gzip_small(data: &[u8]) -> Option<Vec<u8>> {
    SMALL_GZIP.with(|compressor| {
        let mut compressor = compressor.borrow_mut();
        let mut out = vec![0; compressor.gzip_compress_bound(data.len())];
        let len = compressor.gzip_compress(data, &mut out).ok()?;
        out.truncate(len);
        Some(out)
    })
}

fn take_compressor(level: u8) -> Box<CompressorOxide> {
    let pooled = COMPRESSORS.with(|pool| {
        let mut pool = pool.borrow_mut();
        let i = pool.iter().position(|(l, _)| *l == level)?;
        Some(pool.swap_remove(i).1)
    });
    pooled.unwrap_or_else(|| {
        // Raw deflate (no zlib wrapper): gzip adds its own header.
        let flags = create_comp_flags_from_zip_params(i32::from(level), -15, 0);
        Box::new(CompressorOxide::new(flags))
    })
}

fn give_compressor(level: u8, mut c: Box<CompressorOxide>) {
    c.reset();
    COMPRESSORS.with(|pool| {
        let mut pool = pool.borrow_mut();
        if pool.len() < 4 {
            pool.push((level, c));
        }
    });
}

/// `data` as a gzip member (RFC 1952), compressed `CHUNK` bytes at a time
/// with a yield in between. `None` if the compressor fails, which it does
/// not for valid settings. Cancel safe: no I/O or externally visible state is
/// changed before the compressed bytes are returned.
pub(crate) async fn gzip(data: &[u8], level: u8) -> Option<Vec<u8>> {
    #[cfg(feature = "fast-gzip")]
    if level == 1 && data.len() <= SMALL_BODY_LIMIT {
        if let Some(compressed) = gzip_small(data) {
            return Some(compressed);
        }
    }
    gzip_chunked(data, level).await
}

// Cancel safe: all compression state is owned by this future.
async fn gzip_chunked(data: &[u8], level: u8) -> Option<Vec<u8>> {
    let mut c = take_compressor(level);
    let mut out = Vec::with_capacity(data.len() / 4 + 64);
    // Magic, deflate, no flags, no time, extra flags (2: best, 4: fastest),
    // OS 255 (unknown).
    let xfl = match level {
        1 => 4,
        9 => 2,
        _ => 0,
    };
    out.extend_from_slice(&[0x1f, 0x8b, 8, 0, 0, 0, 0, 0, xfl, 255]);
    let mut crc = Crc32::new();
    let mut chunks = data.chunks(CHUNK).peekable();
    if chunks.peek().is_none() {
        finish(&mut c, &[], TDEFLFlush::Finish, &mut out)?;
    }
    while let Some(chunk) = chunks.next() {
        let last = chunks.peek().is_none();
        let flush = if last {
            TDEFLFlush::Finish
        } else {
            TDEFLFlush::None
        };
        finish(&mut c, chunk, flush, &mut out)?;
        crc.update(chunk);
        if !last {
            giong::yield_now().await;
        }
    }
    give_compressor(level, c);
    out.extend_from_slice(&crc.value().to_le_bytes());
    // ISIZE: the length modulo 2^32.
    out.extend_from_slice(&(data.len() as u32).to_le_bytes());
    Some(out)
}

/// Feeds all of `input` to the compressor.
fn finish(
    c: &mut CompressorOxide,
    mut input: &[u8],
    flush: TDEFLFlush,
    out: &mut Vec<u8>,
) -> Option<()> {
    loop {
        let (status, used) = compress_to_output(c, input, flush, |bytes| {
            out.extend_from_slice(bytes);
            true
        });
        input = &input[used..];
        match status {
            TDEFLStatus::Done => return Some(()),
            TDEFLStatus::Okay if input.is_empty() && flush == TDEFLFlush::None => return Some(()),
            TDEFLStatus::Okay => {}
            _ => return None,
        }
    }
}

/// CRC-32 (IEEE), as gzip needs.
struct Crc32(u32);

const CRC_TABLE: [u32; 256] = {
    let mut table = [0u32; 256];
    let mut i = 0;
    while i < 256 {
        let mut c = i as u32;
        let mut k = 0;
        while k < 8 {
            c = if c & 1 != 0 {
                0xedb8_8320 ^ (c >> 1)
            } else {
                c >> 1
            };
            k += 1;
        }
        table[i] = c;
        i += 1;
    }
    table
};

impl Crc32 {
    fn new() -> Self {
        Crc32(!0)
    }

    fn update(&mut self, bytes: &[u8]) {
        let mut c = self.0;
        for &b in bytes {
            c = CRC_TABLE[((c ^ u32::from(b)) & 0xff) as usize] ^ (c >> 8);
        }
        self.0 = c;
    }

    fn value(&self) -> u32 {
        !self.0
    }
}

/// Decompresses a gzip member with miniz_oxide and checks its trailer; for
/// the tests and the fuzz targets.
#[cfg(any(test, fuzzing))]
pub(crate) fn gunzip(gz: &[u8]) -> Vec<u8> {
    assert!(gz.len() >= 18, "{} bytes is too short for gzip", gz.len());
    assert_eq!(&gz[..3], &[0x1f, 0x8b, 8]);
    let body = &gz[10..gz.len() - 8];
    let data = miniz_oxide::inflate::decompress_to_vec(body).expect("valid deflate");
    let n = gz.len();
    let crc = u32::from_le_bytes(gz[n - 8..n - 4].try_into().unwrap());
    let size = u32::from_le_bytes(gz[n - 4..].try_into().unwrap());
    let mut check = Crc32::new();
    check.update(&data);
    assert_eq!((crc, size as usize), (check.value(), data.len()));
    data
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(feature = "fast-gzip")]
    #[test]
    fn fast_gzip_keeps_bytes_and_cooperative_fallback_at_boundary() {
        giong::block_on(async {
            for size in [
                0,
                1,
                1400,
                2700,
                SMALL_BODY_LIMIT,
                SMALL_BODY_LIMIT + 1,
                CHUNK + 1,
            ] {
                let body: Vec<u8> = (0..size).map(|n| (n % 251) as u8).collect();
                for level in [1, 6, 9] {
                    let compressed = gzip(&body, level).await.unwrap();
                    assert_eq!(gunzip(&compressed), body);
                    if size > SMALL_BODY_LIMIT || level != 1 {
                        assert_eq!(compressed, gzip_chunked(&body, level).await.unwrap());
                    }
                }
            }
        });
    }

    #[cfg(feature = "fast-gzip")]
    #[test]
    fn fast_gzip_after_dropping_a_large_compression() {
        use std::{
            future::{poll_fn, Future},
            task::Poll,
        };
        giong::block_on(async {
            let large = vec![b'A'; CHUNK * 3];
            let mut pending = Box::pin(gzip(&large, 1));
            // The large fallback yields after its first chunk. Drop it while
            // suspended and verify that both compressor paths remain usable.
            poll_fn(|cx| {
                assert!(pending.as_mut().poll(cx).is_pending());
                Poll::Ready(())
            })
            .await;
            drop(pending);
            let small = b"subsequent independent response".repeat(64);
            assert_eq!(gunzip(&gzip(&small, 1).await.unwrap()), small);
            assert_eq!(gunzip(&gzip(&large, 1).await.unwrap()), large);
        });
    }

    // Run manually on a pinned Linux core in release mode. Both paths use
    // the same process, corpus, level and warm-up. No network or app data.
    #[cfg(all(feature = "fast-gzip", target_os = "linux"))]
    #[test]
    #[ignore = "synthetic performance benchmark"]
    fn fast_gzip_cpu_profile() {
        use std::{hint::black_box, time::Instant};
        fn ticks() -> (u64, u64) {
            let stat = std::fs::read_to_string("/proc/self/stat").unwrap();
            let f: Vec<_> = stat
                .rsplit_once(')')
                .unwrap()
                .1
                .split_whitespace()
                .collect();
            (f[11].parse().unwrap(), f[12].parse().unwrap())
        }
        let hz: f64 = std::env::var("GIONG_BENCH_HZ").unwrap().parse().unwrap();
        let iterations = 10_000;
        giong::block_on(async {
            for size in [1400, 2700, 5000, 8192, SMALL_BODY_LIMIT] {
                for corpus in ["html", "varied"] {
                    let mut input = Vec::new();
                    let mut x = 0x12345678u32;
                    for n in 0..size {
                        if corpus == "varied" {
                            x ^= x << 13;
                            x ^= x >> 17;
                            x ^= x << 5;
                            input.push(b' ' + (x % 95) as u8);
                        } else {
                            let template = b"<li class=card><h2>Product 0123456789</h2><p>Example response content, details and description.</p></li>";
                            input.push(if n % 79 == 0 {
                                b'0' + ((n / 79) % 10) as u8
                            } else {
                                template[n % template.len()]
                            });
                        }
                    }
                    let legacy = gzip_chunked(&input, 1).await.unwrap();
                    let fast = gzip(&input, 1).await.unwrap();
                    assert_eq!(gunzip(&legacy), input);
                    assert_eq!(gunzip(&fast), input);
                    for round in 0..6 {
                        for fast_backend in if round % 2 == 0 {
                            [false, true]
                        } else {
                            [true, false]
                        } {
                            for _ in 0..250 {
                                let result = if fast_backend {
                                    gzip(black_box(&input), 1).await
                                } else {
                                    gzip_chunked(black_box(&input), 1).await
                                };
                                black_box(result.unwrap());
                            }
                            let before = ticks();
                            let begin = Instant::now();
                            let mut times = Vec::with_capacity(iterations);
                            let mut bytes = 0usize;
                            for _ in 0..iterations {
                                let started = Instant::now();
                                let result = if fast_backend {
                                    gzip(black_box(&input), 1).await
                                } else {
                                    gzip_chunked(black_box(&input), 1).await
                                }
                                .unwrap();
                                times.push(started.elapsed().as_nanos());
                                bytes += black_box(result.len());
                            }
                            let elapsed = begin.elapsed();
                            let after = ticks();
                            times.sort_unstable();
                            println!("gzip_profile,{size},{corpus},{round},{},{iterations},{:.3},{:.3},{},{},{:.3}",
                                if fast_backend {"fast"} else {"legacy"},
                                elapsed.as_secs_f64()*1e6/iterations as f64,
                                (after.0+after.1-before.0-before.1) as f64*1e6/hz/iterations as f64,
                                times[iterations/2],times[iterations*99/100],
                                bytes as f64/iterations as f64);
                        }
                    }
                }
            }
        });
    }

    #[test]
    fn the_crc_matches_the_standard_check_value() {
        let mut c = Crc32::new();
        c.update(b"123456789");
        assert_eq!(c.value(), 0xcbf4_3926);
    }

    #[test]
    fn gzip_round_trips_at_every_level_and_size() {
        let text: Vec<u8> = (0..300_000u32)
            .flat_map(|i| format!("{{\"id\":{i},\"name\":\"user {i}\"}},").into_bytes())
            .take(300_000)
            .collect();
        giong::block_on(async {
            for level in [1, 6, 9] {
                for len in [0, 1, 1000, CHUNK, CHUNK + 1, text.len()] {
                    let gz = gzip(&text[..len], level).await.unwrap();
                    assert_eq!(gunzip(&gz), &text[..len], "level {level}, {len} bytes");
                }
            }
        });
    }

    #[test]
    fn accept_encoding_is_read_with_q_values() {
        assert!(accepts_gzip("gzip"));
        assert!(accepts_gzip("br, gzip;q=0.8, deflate"));
        assert!(accepts_gzip("GZIP"));
        assert!(accepts_gzip("*"));
        assert!(accepts_gzip("x-gzip"));
        assert!(!accepts_gzip("gzip;q=0"));
        assert!(!accepts_gzip("gzip; q=0.000, *"));
        assert!(!accepts_gzip("br, deflate"));
        assert!(!accepts_gzip("*;q=0"));
        assert!(!accepts_gzip("identity"));
    }

    #[test]
    fn text_types_are_compressible_and_others_not() {
        for t in [
            "text/html; charset=utf-8",
            "application/json",
            "application/problem+json",
            "image/svg+xml",
            "application/x-ndjson",
            "Application/JSON",
        ] {
            assert!(compressible(t), "{t}");
        }
        for t in [
            "image/png",
            "application/octet-stream",
            "text/event-stream",
            "video/mp4",
        ] {
            assert!(!compressible(t), "{t}");
        }
    }
}
