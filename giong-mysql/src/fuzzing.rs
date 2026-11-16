//! Checks shared by the randomised tests (`fuzz_tests.rs`) and the
//! coverage-guided fuzz targets (`giong-mysql/fuzz`): whatever a server
//! (or a URL) sends, the parsers return a verdict without panicking,
//! framing respects its limit, and a connection with requests in flight
//! ends every one of them and writes only well-formed commands.

use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::future::{poll_fn, Future};
use std::io;
use std::net::Shutdown;
use std::pin::{pin, Pin};
use std::rc::Rc;
use std::sync::{mpsc, Mutex, OnceLock};
use std::task::Poll;
use std::time::SystemTime;

use crate::client::Client;
use crate::config::Config;
use crate::connect::{Message, ReadBuf};
use crate::error::DbError;
use crate::protocol;
use crate::row::{Column, RowsBuilder};
use crate::types::{Bits, ColumnType};

/// Runs one packet's payload through every parser of server packets.
pub fn check_payload(input: &[u8]) {
    let _ = protocol::handshake(input);
    let _ = protocol::ok(input);
    let _ = protocol::err(input);
    let _ = DbError::parse(input);
    let _ = protocol::column_def(input);
    let _ = protocol::prepare_ok(input);
    let _ = protocol::is_end_of_rows(input);
}

/// Frames `input` into packets, fed in pieces whose sizes `piece` picks.
pub(crate) fn framing(input: &[u8], limit: usize, mut piece: impl FnMut(usize) -> usize) {
    let mut buf = ReadBuf::new();
    let mut at = 0;
    while at < input.len() {
        let n = piece(input.len() - at).clamp(1, input.len() - at);
        buf.extend(&input[at..at + n]);
        at += n;
        while let Some(m) = buf.next(limit) {
            if let Message::Here { payload, .. } = m {
                assert!(payload.len() <= limit);
                let _ = protocol::ok(buf.bytes(payload.clone()));
                buf.consume(&payload);
            }
        }
    }
}

/// Fuzz entry: a byte choosing the size limit, a byte setting how the
/// input is cut, then the bytes a server sent.
pub fn check_framing(data: &[u8]) {
    let [limit, cut, input @ ..] = data else {
        return;
    };
    let limit = [16, 100, 1 << 20][usize::from(*limit) % 3];
    let mut state = u64::from(*cut) | 1;
    framing(input, limit, move |left| {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        1 + (state as usize) % left
    });
}

/// Columns of every type, signed and unsigned, text and binary.
pub(crate) fn all_columns() -> Vec<Column> {
    let mut cols = Vec::new();
    for code in [
        0u8, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 15, 16, 242, 245, 246, 247, 248, 249, 250,
        251, 252, 253, 254, 255, 100,
    ] {
        for (flags, collation) in [(0, 255), (32, 63)] {
            cols.push(Column::for_tests(
                "c",
                ColumnType::from_code(code),
                flags,
                collation,
            ));
        }
    }
    cols
}

/// Reads a row payload with the given columns, and every value in it as
/// every type.
pub(crate) fn rows(payload: &[u8], cols: &[Column]) {
    let n = cols.len();
    let mut b = RowsBuilder::default();
    if b.push(payload, cols, 1 << 20).is_err() {
        return;
    }
    for row in b.finish(cols.to_vec().into()).into_rows() {
        for i in 0..n {
            let _ = row.get::<i64>(i);
            let _ = row.get::<u64>(i);
            let _ = row.get::<i8>(i);
            let _ = row.get::<bool>(i);
            let _ = row.get::<f64>(i);
            let _ = row.get::<f32>(i);
            let _ = row.get::<&str>(i);
            let _ = row.get::<String>(i);
            let _ = row.get::<&[u8]>(i);
            let _ = row.get::<Bits>(i);
            let _ = row.get::<SystemTime>(i);
            let _ = row.get::<Option<String>>(i);
        }
        let _ = format!("{row:?}");
    }
}

/// Fuzz entry: a byte with the number of columns (1 to 4), a byte for each
/// column's type, then a row payload.
pub fn check_rows(data: &[u8]) {
    let Some((&n, rest)) = data.split_first() else {
        return;
    };
    let n = 1 + usize::from(n) % 4;
    if rest.len() < n {
        return;
    }
    let all = all_columns();
    let cols: Vec<Column> = rest[..n]
        .iter()
        .map(|&b| all[usize::from(b) % all.len()].clone())
        .collect();
    rows(&rest[n..], &cols);
}

/// Fuzz entry: a connection URL.
pub fn check_url(data: &[u8]) {
    if let Ok(config) = Config::from_url(&String::from_utf8_lossy(data)) {
        let _ = format!("{config:?}");
    }
}

/// A server played from bytes: each read gets the next piece of what it
/// sends (nothing, once they run out: the server closed the connection),
/// and what the client writes is kept.
pub(crate) struct Memory {
    input: RefCell<VecDeque<Vec<u8>>>,
    output: Rc<RefCell<Vec<u8>>>,
    closed: Cell<bool>,
}

impl Memory {
    pub(crate) fn new(input: VecDeque<Vec<u8>>, output: Rc<RefCell<Vec<u8>>>) -> Self {
        Self {
            input: RefCell::new(input),
            output,
            closed: Cell::new(false),
        }
    }

    pub(crate) async fn recv(&self) -> io::Result<Vec<u8>> {
        // Other tasks run between reads, as they would while a socket
        // waits: tasks woken by an answer send their next request.
        giong::yield_now().await;
        if self.closed.get() {
            return Ok(Vec::new());
        }
        Ok(self.input.borrow_mut().pop_front().unwrap_or_default())
    }

    pub(crate) async fn write_all(&self, buf: Vec<u8>) -> (io::Result<()>, Vec<u8>) {
        if self.closed.get() {
            return (Err(io::ErrorKind::BrokenPipe.into()), buf);
        }
        self.output.borrow_mut().extend_from_slice(&buf);
        (Ok(()), buf)
    }

    pub(crate) fn shutdown(&self, how: Shutdown) {
        if how != Shutdown::Write {
            self.closed.set(true);
        }
    }
}

#[cfg(test)]
/// `bytes` cut into pieces whose sizes `seed` picks (0: one piece).
pub(crate) fn pieces(seed: u8, bytes: &[u8]) -> VecDeque<Vec<u8>> {
    let mut out = VecDeque::new();
    if seed == 0 {
        out.push_back(bytes.to_vec());
        return out;
    }
    let mut state = u32::from(seed);
    let mut rest = bytes;
    while !rest.is_empty() {
        state ^= state << 13;
        state ^= state >> 17;
        state ^= state << 5;
        let n = (1 + state as usize % 48).min(rest.len());
        out.push_back(rest[..n].to_vec());
        rest = &rest[n..];
    }
    out
}

/// Runs futures to completion together.
async fn join_all(mut futures: Vec<Pin<Box<dyn Future<Output = ()> + '_>>>) {
    poll_fn(|cx| {
        futures.retain_mut(|f| f.as_mut().poll(cx).is_pending());
        if futures.is_empty() {
            Poll::Ready(())
        } else {
            Poll::Pending
        }
    })
    .await
}

/// What a session asks of the server, as the bits of two bytes choose:
/// statements from several tasks at once (pipelined), one given up after
/// it was sent, a batch, a reset of the session, the connection shared as
/// in a pool, then a transaction. Returns what the client wrote.
pub(crate) async fn session(ops: [u8; 2], server: VecDeque<Vec<u8>>) -> Vec<u8> {
    let [ops, tx] = ops;
    let output = Rc::new(RefCell::new(Vec::new()));
    let mut config = Config::new().host("memory").user("u").max_result_size(64);
    if ops & 0x20 != 0 {
        config = config.statement_cache_capacity(0);
    }
    let memory = Memory::new(server, output.clone());
    let mut client = Client::over_memory(memory, config, ops & 0x80 != 0);
    client.set_shared(ops & 0x10 != 0);
    {
        let c = &client;
        let mut work: Vec<Pin<Box<dyn Future<Output = ()> + '_>>> = Vec::new();
        if ops & 0x01 != 0 {
            work.push(Box::pin(async move {
                if let Ok(rows) = c.query("SELECT ?", &[&1i64]).await {
                    for row in &rows {
                        let _ = row.get::<i64>(0);
                        let _ = row.get::<&str>(0);
                    }
                }
            }));
        }
        if ops & 0x02 != 0 {
            work.push(Box::pin(async move {
                let _ = c.execute("UPDATE t SET a = ?", &[&"x"]).await;
                let _ = c.insert("INSERT INTO t VALUES (?)", &[&2u8]).await;
            }));
        }
        if ops & 0x04 != 0 {
            work.push(Box::pin(async move {
                let _ = c.batch_execute("SET @a = 1; SELECT 1").await;
            }));
        }
        if ops & 0x08 != 0 {
            work.push(Box::pin(async move {
                let _ = c.query_opt("SELECT a FROM t", &[]).await;
                let _ = c.reset_session().await;
            }));
        }
        if ops & 0x40 != 0 {
            // Sent, then given up before its answer.
            work.push(Box::pin(async move {
                let mut query = pin!(c.query("SELECT 2", &[]));
                poll_fn(|cx| {
                    let _ = query.as_mut().poll(cx);
                    Poll::Ready(())
                })
                .await;
            }));
        }
        join_all(work).await;
    }
    if tx & 0x03 != 0 {
        if let Ok(t) = client.transaction().await {
            let _ = t.execute("DELETE FROM t WHERE a = ?", &[&3i32]).await;
            if tx & 0x04 != 0 {
                let mut late = pin!(t.execute("SELECT 4", &[]));
                poll_fn(|cx| {
                    let _ = late.as_mut().poll(cx);
                    Poll::Ready(())
                })
                .await;
            }
            match tx & 0x03 {
                1 => drop(t.commit().await),
                2 => drop(t.rollback().await),
                _ => drop(t),
            }
        }
    }
    // The server's bytes run out, and then the connection ends: nothing
    // may be left waiting.
    while !client.is_closed() {
        giong::yield_now().await;
    }
    assert_eq!(client.in_flight(), 0, "requests left waiting");
    drop(client);
    let written = output.take();
    check_commands(&written);
    written
}

/// What the client wrote is a sequence of whole command packets, each the
/// first of its exchange.
fn check_commands(mut out: &[u8]) {
    while !out.is_empty() {
        let (len, seq) = protocol::header(out).expect("a whole packet header");
        assert_eq!(seq, 0, "a command starts its exchange");
        assert!(len > 0 && out.len() >= 4 + len, "a whole packet");
        assert!(
            [0x01, 0x02, 0x03, 0x0E, 0x16, 0x17, 0x19, 0x1F].contains(&out[4]),
            "command {:#x}",
            out[4]
        );
        out = &out[4 + len..];
    }
}

/// The reads a server's bytes arrive in, from `[n, n bytes, n, n bytes,
/// ...]` (the last one may be short).
pub(crate) fn reads(mut bytes: &[u8]) -> VecDeque<Vec<u8>> {
    let mut out = VecDeque::new();
    while let Some((&n, rest)) = bytes.split_first() {
        let n = usize::from(n).min(rest.len());
        out.push_back(rest[..n].to_vec());
        bytes = &rest[n..];
    }
    out
}

#[cfg(test)]
/// The other way: `exchanges` as reads, each answer starting a read of its
/// own (a server answers only what it was sent).
pub(crate) fn as_reads(exchanges: &[Vec<u8>]) -> Vec<u8> {
    let mut out = Vec::new();
    for answer in exchanges {
        for piece in answer.chunks(255) {
            out.push(piece.len() as u8);
            out.extend_from_slice(piece);
        }
    }
    out
}

/// Fuzz entry: two bytes choosing what the client asks, then the server's
/// bytes as the reads they arrive in (see [`reads`]).
pub fn check_session(data: &[u8]) {
    let [ops, tx, server @ ..] = data else {
        return;
    };
    let job = (reads(server), [*ops, *tx]);
    let (done, result) = mpsc::channel();
    if runtime().lock().unwrap().send((job, done)).is_err() {
        panic!("the runtime thread died");
    }
    result.recv().expect("the runtime thread died");
}

type Job = ((VecDeque<Vec<u8>>, [u8; 2]), mpsc::Sender<()>);

/// One runtime for the whole fuzzing process: setting up an io_uring per
/// input would take most of the time.
fn runtime() -> &'static Mutex<giong::sync::mpsc::Sender<Job>> {
    static JOBS: OnceLock<Mutex<giong::sync::mpsc::Sender<Job>>> = OnceLock::new();
    JOBS.get_or_init(|| {
        let (tx, mut rx) = giong::sync::mpsc::unbounded::<Job>();
        std::thread::spawn(move || {
            giong::Builder::new()
                .workers(1)
                .block_on(async move {
                    while let Some(((server, ops), done)) = rx.recv().await {
                        session(ops, server).await;
                        let _ = done.send(());
                    }
                })
                .expect("runtime");
        });
        Mutex::new(tx)
    })
}

#[cfg(test)]
/// Packets a MySQL 8 server sends, for scripted sessions.
pub(crate) mod server {
    use crate::protocol;

    pub fn packet(out: &mut Vec<u8>, seq: u8, payload: &[u8]) {
        protocol::packet(out, seq, |o| o.extend_from_slice(payload));
    }

    pub fn ok(status: u16) -> Vec<u8> {
        let mut p = vec![0, 0, 0];
        p.extend_from_slice(&status.to_le_bytes());
        p.extend_from_slice(&[0, 0]);
        p
    }

    /// The OK packet that ends a result set (`CLIENT_DEPRECATE_EOF`).
    pub fn end_of_rows(status: u16) -> Vec<u8> {
        let mut p = vec![0xFE, 0, 0];
        p.extend_from_slice(&status.to_le_bytes());
        p.extend_from_slice(&[0, 0]);
        p
    }

    pub fn error(code: u16, message: &str) -> Vec<u8> {
        let mut p = vec![0xFF];
        p.extend_from_slice(&code.to_le_bytes());
        p.extend_from_slice(b"#HY000");
        p.extend_from_slice(message.as_bytes());
        p
    }

    /// A `BIGINT` column named `c`.
    pub fn column() -> Vec<u8> {
        let mut p = Vec::new();
        for s in ["def", "", "", "", "c", ""] {
            p.push(s.len() as u8);
            p.extend_from_slice(s.as_bytes());
        }
        p.push(0x0C);
        p.extend_from_slice(&63u16.to_le_bytes());
        p.extend_from_slice(&20u32.to_le_bytes());
        p.push(8); // LONGLONG
        p.extend_from_slice(&0u16.to_le_bytes());
        p.extend_from_slice(&[0, 0, 0]);
        p
    }

    /// The answer to `COM_STMT_PREPARE`: statement `id`, with `params`
    /// parameters and one `BIGINT` column.
    pub fn prepared(out: &mut Vec<u8>, id: u32, params: u16) {
        let mut p = vec![0];
        p.extend_from_slice(&id.to_le_bytes());
        p.extend_from_slice(&1u16.to_le_bytes());
        p.extend_from_slice(&params.to_le_bytes());
        p.extend_from_slice(&[0, 0, 0]);
        packet(out, 1, &p);
        for seq in (2..).take(usize::from(params) + 1) {
            packet(out, seq, &column());
        }
    }

    /// The answer to `COM_STMT_EXECUTE`: a result of `count` columns
    /// announced, one described, and one row holding `value`.
    pub fn result(out: &mut Vec<u8>, count: &[u8], value: i64) {
        packet(out, 1, count);
        packet(out, 2, &column());
        let mut row = vec![0, 0];
        row.extend_from_slice(&value.to_le_bytes());
        packet(out, 3, &row);
        packet(out, 4, &end_of_rows(protocol::status::AUTOCOMMIT));
    }

    /// One answer: a single packet.
    pub fn single(payload: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        packet(&mut out, 1, payload);
        out
    }

    /// A text-protocol result of one column and one row holding `value`,
    /// ending with `status`.
    pub fn text_result(out: &mut Vec<u8>, first: u8, value: &str, status: u16) {
        packet(out, first, &[1]);
        packet(out, first + 1, &column());
        let mut row = vec![value.len() as u8];
        row.extend_from_slice(value.as_bytes());
        packet(out, first + 2, &row);
        packet(out, first + 3, &end_of_rows(status));
    }
}
