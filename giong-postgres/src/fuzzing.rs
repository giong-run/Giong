//! Checks shared by the randomised tests (`fuzz_tests.rs`) and the
//! coverage-guided fuzz targets (`giong-postgres/fuzz`): whatever a server
//! (or a URL) sends, the parsers return a verdict without panicking, and a
//! connection settles every statement without panicking or hanging.

use std::future::{poll_fn, Future};
use std::pin::pin;
use std::rc::Rc;
use std::task::Poll;
use std::time::Duration;

use giong::net::UnixStream;

use crate::auth::Scram;
use crate::client::Client;
use crate::config::Config;
use crate::connect::{ReadBuf, Startup, Stream};
use crate::error::{DbError, Error};
use crate::protocol::{data_row, parse_backend};
use crate::row::RowsBuilder;

/// Frames `input` into messages, delivered in two pieces cut at `cut`, and
/// runs every message through every parser that reads server messages.
pub(crate) fn messages(input: &[u8], cut: usize) {
    let cut = cut.min(input.len());
    let mut buf = ReadBuf::new();
    buf.extend(&input[..cut]);
    let mut fed_rest = false;
    loop {
        let Ok(Some((tag, len))) = buf.peek() else {
            if fed_rest {
                break;
            }
            buf.extend(&input[cut..]);
            fed_rest = true;
            continue;
        };
        if len > 1 << 20 {
            buf.skip(len);
            continue;
        }
        let Some(body) = buf.body(len) else {
            if fed_rest {
                break;
            }
            buf.extend(&input[cut..]);
            fed_rest = true;
            continue;
        };
        let bytes = buf.bytes(body).to_vec();
        buf.consume(len);
        if let Ok(msg) = parse_backend(tag, &bytes) {
            let _ = format!("{msg:?}");
        }
        let _ = DbError::parse(&bytes);
        let mut rows = RowsBuilder::default();
        let _ = rows.push(&bytes, 2, 1 << 16);
        let _ = data_row(&bytes, 1, |_| {});
    }
}

/// Fuzz entry: two bytes saying where the input is cut, then the bytes a
/// server sent.
pub fn check_messages(data: &[u8]) {
    let [a, b, input @ ..] = data else {
        return;
    };
    messages(input, usize::from(u16::from_le_bytes([*a, *b])));
}

/// Fuzz entry: a connection URL.
pub fn check_url(data: &[u8]) {
    if let Ok(url) = std::str::from_utf8(data) {
        if let Ok(config) = Config::from_url(url) {
            let _ = format!("{config:?}");
        }
    }
}

/// Runs a SCRAM exchange against a server's first and final messages.
pub(crate) fn scram(server_first: &[u8], server_final: &[u8]) {
    // Keep the key derivation cheap: the iteration limit itself is tested
    // in the auth module.
    let text = String::from_utf8_lossy(server_first);
    let iterations = text.rsplit("i=").next().and_then(|i| i.parse::<u64>().ok());
    if iterations.is_some_and(|i| i > 4096) {
        return;
    }
    let mut scram = Scram::with_nonce("pw", "", "abcdef");
    let _ = scram.client_final(server_first);
    let _ = scram.finish(server_final);
}

/// Fuzz entry: the server's first SCRAM message, a 0xff byte, and its
/// final message.
pub fn check_scram(data: &[u8]) {
    let (first, last) = match data.iter().position(|&b| b == 0xff) {
        Some(i) => (&data[..i], &data[i + 1..]),
        None => (data, &[][..]),
    };
    scram(first, last);
}

/// What [`session`] does, besides the three statements it always runs.
pub(crate) mod flags {
    /// The connection is one a pool shares (a transaction left open closes it).
    pub const SHARED: u8 = 1;
    /// The pool's session reset, then one more statement.
    pub const RESET: u8 = 2;
    /// A statement cache of one entry instead of eight, so statements are
    /// evicted and closed.
    pub const TINY_CACHE: u8 = 4;
    /// A fourth statement whose future is dropped while it is being prepared.
    pub const DROPPED: u8 = 8;
    /// A transaction: `BEGIN`, a statement, `COMMIT`.
    pub const TRANSACTION: u8 = 16;
}

/// How long the fake server waits for the client's next request before it
/// closes the connection. Waiting longer only makes a session slower: an
/// early close is a server going away, which the client must handle too.
const SERVER_PATIENCE: Duration = Duration::from_millis(10);

/// Plays a server on `sock` that answers each request of the client (each
/// `Sync` and each simple query) with the next of `answers`, as it is,
/// whether or not it makes sense. It closes the connection when the answers
/// run out, when the client says goodbye, or when the client stops asking.
async fn serve(sock: UnixStream, answers: Vec<Vec<u8>>) {
    let mut answers = answers.into_iter();
    let mut pending = Vec::new();
    loop {
        let mut requests = 0;
        while pending.len() >= 5 {
            let len = i32::from_be_bytes([pending[1], pending[2], pending[3], pending[4]]);
            let len = usize::try_from(len).unwrap_or(0).max(4);
            if pending.len() < 1 + len {
                break;
            }
            let tag = pending[0];
            pending.drain(..1 + len);
            match tag {
                b'S' | b'Q' => requests += 1,
                b'X' => return,
                _ => {}
            }
        }
        for _ in 0..requests {
            let Some(answer) = answers.next() else {
                return;
            };
            if !answer.is_empty() && sock.write_all(answer).await.0.is_err() {
                return;
            }
        }
        match giong::time::timeout(SERVER_PATIENCE, sock.recv()).await {
            Ok(Ok(chunk)) if !chunk.is_empty() => pending.extend_from_slice(&chunk),
            _ => return,
        }
    }
}

/// Runs statements on a connection to a fake server that answers with
/// `answers` (see [`serve`]) and returns what each statement got.
///
/// # Panics
///
/// Panics when a statement still waits 5 seconds after the session started
/// (the server gives up long before), or when the connection is closed with
/// requests still counted as in flight.
pub(crate) async fn session(flags: u8, answers: Vec<Vec<u8>>) -> Vec<Result<(), Error>> {
    let (ours, theirs) = UnixStream::pair().expect("a socket pair");
    let config = Config::new()
        .max_result_size(4096)
        .statement_cache_capacity(if flags & flags::TINY_CACHE != 0 { 1 } else { 8 });
    let startup = Startup {
        stream: Stream::Unix(ours),
        buf: ReadBuf::new(),
        params: vec![
            ("server_version".into(), "16.4".into()),
            ("in_hot_standby".into(), "off".into()),
        ],
        key: Some((1, 2)),
        status: b'I',
    };
    let client = Rc::new(Client::start(startup, config));
    if flags & flags::SHARED != 0 {
        client.set_shared(true);
    }
    let server = giong::spawn(serve(theirs, answers));
    let run = async {
        let mut tasks = Vec::new();
        let c = client.clone();
        tasks.push(giong::spawn(async move {
            let rows = c
                .query("SELECT $1::int4 AS a, $2 AS b", &[&7i32, &"x"])
                .await?;
            for row in &rows {
                let _: Option<i32> = row.get(0)?;
                let _: Option<&str> = row.get(1)?;
            }
            Ok(())
        }));
        let c = client.clone();
        tasks.push(giong::spawn(async move {
            c.execute("UPDATE t SET a = 1", &[]).await.map(|_| ())
        }));
        let c = client.clone();
        tasks.push(giong::spawn(async move {
            c.batch_execute("SET x = 1; SELECT 1").await
        }));
        if flags & flags::DROPPED != 0 {
            let c = client.clone();
            tasks.push(giong::spawn(async move {
                let mut query = pin!(c.query("SELECT pg_sleep($1)", &[&1i32]));
                poll_fn(|cx| {
                    let _ = query.as_mut().poll(cx);
                    Poll::Ready(())
                })
                .await;
                Ok(())
            }));
        }
        let mut outcomes = Vec::new();
        for task in tasks {
            outcomes.push(task.await.expect("a statement task panicked"));
        }
        let mut client = Rc::try_unwrap(client).expect("the tasks are done");
        if flags & flags::TRANSACTION != 0 {
            outcomes.push(
                async {
                    let tx = client.transaction().await?;
                    tx.execute("UPDATE t SET a = 1", &[]).await?;
                    tx.commit().await
                }
                .await,
            );
        }
        if flags & flags::RESET != 0 {
            client.begin_lease();
            client.reset_session();
            client.end_lease();
            outcomes.push(client.query_one("SELECT 1", &[]).await.map(|_| ()));
        }
        if client.is_closed() {
            assert_eq!(
                client.in_flight(),
                0,
                "a closed connection still counts requests"
            );
        }
        outcomes
    };
    let outcomes = giong::time::timeout(Duration::from_secs(5), run)
        .await
        .unwrap_or_else(|_| {
            panic!("a statement still waits 5 s into the session (flags {flags:#x})")
        });
    let _ = server.await;
    outcomes
}

/// Splits fuzz input into answers: each is a little-endian `u16` length and
/// that many bytes (fewer at the end).
pub(crate) fn split_answers(mut data: &[u8]) -> Vec<Vec<u8>> {
    let mut answers = Vec::new();
    while let [a, b, rest @ ..] = data {
        let n = usize::from(u16::from_le_bytes([*a, *b])).min(rest.len());
        answers.push(rest[..n].to_vec());
        data = &rest[n..];
    }
    answers
}

/// Fuzz entry: a byte of [`flags`], then the server's answers (see
/// [`split_answers`]).
pub fn check_session(data: &[u8]) {
    let [flags, answers @ ..] = data else {
        return;
    };
    giong::block_on(session(*flags, split_answers(answers)));
}
