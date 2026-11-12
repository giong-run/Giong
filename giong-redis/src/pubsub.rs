//! Pub/Sub: a connection that receives the messages of the channels it
//! subscribed to.

use std::collections::VecDeque;

use crate::cmd::{cmd, Cmd};
use crate::config::Config;
use crate::connect::{self, Stream};
use crate::error::{Error, ErrorKind};
use crate::resp::{Next, ReadBuf};
use crate::value::{ToArg, Value};

/// A connection subscribed to channels, receiving their messages.
///
/// ```no_run
/// use giong_redis::{Config, PubSub};
///
/// # async fn f(config: &Config) -> Result<(), giong_redis::Error> {
/// let mut sub = PubSub::connect(config).await?;
/// sub.subscribe("orders").await?;
/// sub.psubscribe("user:*:events").await?;
/// loop {
///     let message = sub.next_message().await?;
///     println!("{:?}: {}", message.channel(), message.payload_str()?);
/// }
/// # }
/// ```
///
/// Messages are published with [`Client::publish`](crate::Client::publish)
/// (or `PUBLISH` from anywhere). A subscribed connection does nothing else,
/// which is why it is a type of its own.
///
/// Messages sent while the connection is down are lost: when
/// [`next_message`](Self::next_message) fails with
/// [`ErrorKind::Closed`], connect again and subscribe again.
pub struct PubSub {
    stream: Stream,
    buf: ReadBuf,
    config: Config,
    /// Messages that arrived while waiting for a confirmation.
    pending: VecDeque<Message>,
    broken: Option<Error>,
    id: i64,
}

/// A message received on a channel.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Message {
    channel: Vec<u8>,
    pattern: Option<Vec<u8>>,
    payload: Vec<u8>,
}

impl Message {
    /// The channel it was published on.
    pub fn channel(&self) -> &[u8] {
        &self.channel
    }

    /// The pattern it matched, for a subscription made with
    /// [`PubSub::psubscribe`].
    pub fn pattern(&self) -> Option<&[u8]> {
        self.pattern.as_deref()
    }

    /// What was published.
    pub fn payload(&self) -> &[u8] {
        &self.payload
    }

    /// What was published, as text.
    pub fn payload_str(&self) -> Result<&str, Error> {
        std::str::from_utf8(&self.payload).map_err(|_| {
            Error::conversion(format!(
                "the message on {:?} is {} bytes that are not UTF-8; read it with payload()",
                String::from_utf8_lossy(&self.channel),
                self.payload.len()
            ))
        })
    }

    /// What was published, taken out of the message.
    pub fn into_payload(self) -> Vec<u8> {
        self.payload
    }
}

impl PubSub {
    /// Opens a connection for subscriptions, logged in as
    /// [`Client::connect`](crate::Client::connect) does.
    ///
    /// # Panics
    ///
    /// Panics when called outside of a giong runtime.
    pub async fn connect(config: &Config) -> Result<PubSub, Error> {
        let startup = connect::connect(config).await?;
        Ok(PubSub {
            stream: startup.stream,
            buf: startup.buf,
            config: config.clone(),
            pending: VecDeque::new(),
            broken: None,
            id: startup.server.id,
        })
    }

    /// The connection's id on the server (`CLIENT ID`).
    pub fn connection_id(&self) -> i64 {
        self.id
    }

    /// Subscribes to a channel (`SUBSCRIBE`); returns once the server has
    /// confirmed it.
    pub async fn subscribe(&mut self, channel: impl ToArg) -> Result<(), Error> {
        self.call(cmd("SUBSCRIBE").arg(channel), "subscribe").await
    }

    /// Subscribes to every channel whose name matches a glob-style pattern
    /// (`PSUBSCRIBE`), such as `news.*`.
    pub async fn psubscribe(&mut self, pattern: impl ToArg) -> Result<(), Error> {
        self.call(cmd("PSUBSCRIBE").arg(pattern), "psubscribe")
            .await
    }

    /// Ends a subscription made with [`subscribe`](Self::subscribe).
    pub async fn unsubscribe(&mut self, channel: impl ToArg) -> Result<(), Error> {
        self.call(cmd("UNSUBSCRIBE").arg(channel), "unsubscribe")
            .await
    }

    /// Ends a subscription made with [`psubscribe`](Self::psubscribe).
    pub async fn punsubscribe(&mut self, pattern: impl ToArg) -> Result<(), Error> {
        self.call(cmd("PUNSUBSCRIBE").arg(pattern), "punsubscribe")
            .await
    }

    /// The next message, waiting as long as it takes. It can be cancelled
    /// (with a timeout, or in a `select`) without losing a message.
    pub async fn next_message(&mut self) -> Result<Message, Error> {
        loop {
            if let Some(m) = self.pending.pop_front() {
                return Ok(m);
            }
            if let Some(m) = self.read_one(None)? {
                return Ok(m);
            }
            self.receive().await?;
        }
    }

    /// Sends a subscription command and waits for its confirmation.
    async fn call(&mut self, command: Cmd, kind: &str) -> Result<(), Error> {
        self.check_open()?;
        let mut out = Vec::new();
        command.encode(&mut out);
        // A write cut short by cancellation would leave half a command on
        // the connection: until the write ends, the connection counts as
        // broken.
        self.broken = Some(Error::closed(
            "a subscription command was cancelled while it was being written, so the \
             connection was given up; connect again",
        ));
        let (written, _) = self.stream.write_all(out).await;
        if let Err(e) = written {
            let e = Error::io(
                ErrorKind::Closed,
                format!("writing to {} failed", self.config.target()),
                e,
            );
            self.broken = Some(e.clone());
            return Err(e);
        }
        self.broken = None;
        let timeout = self.config.response_timeout;
        let wait = async {
            loop {
                if let Some(confirmed) = self.read_confirmation(kind)? {
                    return confirmed;
                }
                self.receive().await?;
            }
        };
        match giong::time::timeout(timeout, wait).await {
            Ok(result) => result,
            Err(_) => Err(Error::new(
                ErrorKind::Timeout,
                format!(
                    "{} did not confirm {} within the response timeout of {timeout:?}",
                    self.config.target(),
                    command.name()
                ),
            )),
        }
    }

    fn check_open(&self) -> Result<(), Error> {
        match &self.broken {
            Some(e) => Err(e.clone()),
            None => Ok(()),
        }
    }

    /// Reads replies until the confirmation of `kind` (or an error reply);
    /// messages on the way are kept for [`next_message`](Self::next_message).
    fn read_confirmation(&mut self, kind: &str) -> Result<Option<Result<(), Error>>, Error> {
        loop {
            let mut confirmed = None;
            let message = self.read_one(Some((kind, &mut confirmed)))?;
            if let Some(m) = message {
                self.pending.push_back(m);
                continue;
            }
            return Ok(confirmed);
        }
    }

    /// Reads the next whole reply in the buffer: a message is returned; a
    /// confirmation of `kind` or an error reply is put in `confirmed` (when
    /// asked for); anything else is passed over. `None` when nothing whole
    /// is left, or a confirmation was found.
    fn read_one(
        &mut self,
        mut confirm: Option<(&str, &mut Option<Result<(), Error>>)>,
    ) -> Result<Option<Message>, Error> {
        loop {
            let value = match self.buf.next(self.config.max_reply_size) {
                Err(e) => {
                    self.broken = Some(e.clone());
                    return Err(e);
                }
                Ok(Next::Pending) => return Ok(None),
                Ok(Next::TooLarge) => {
                    return Err(Error::new(
                        ErrorKind::TooLarge,
                        format!(
                            "a message larger than the limit of {} bytes was thrown away; raise \
                             Config::max_reply_size",
                            self.config.max_reply_size
                        ),
                    ))
                }
                Ok(Next::Ready(n)) => self.buf.take(n).inspect_err(|e| {
                    self.broken = Some(e.clone());
                })?,
            };
            match value {
                Value::Push(items) => {
                    let kind = items.first().and_then(Value::as_str).unwrap_or_default();
                    match (kind, items.len()) {
                        ("message", 3) => return Ok(Some(message(items, false))),
                        ("pmessage", 4) => return Ok(Some(message(items, true))),
                        (k, _) => {
                            if let Some((want, slot)) = confirm.as_mut() {
                                if k == *want {
                                    **slot = Some(Ok(()));
                                    return Ok(None);
                                }
                            }
                        }
                    }
                }
                Value::Error(e) => {
                    if let Some((_, slot)) = confirm.as_mut() {
                        **slot = Some(Err(crate::client::server_error(e)));
                        return Ok(None);
                    }
                }
                _ => {}
            }
        }
    }

    /// Waits for more bytes.
    async fn receive(&mut self) -> Result<(), Error> {
        self.check_open()?;
        let chunk = self.stream.recv().await;
        let err = match chunk {
            Ok(c) if c.is_empty() => Error::closed(format!(
                "the server at {} closed the connection; connect and subscribe again",
                self.config.target()
            )),
            Ok(c) => {
                self.buf.extend(&c);
                return Ok(());
            }
            Err(e) => Error::io(
                ErrorKind::Closed,
                format!("reading from {} failed", self.config.target()),
                e,
            ),
        };
        self.broken = Some(err.clone());
        Err(err)
    }
}

fn bytes(v: Value) -> Vec<u8> {
    match v {
        Value::Bulk(b) => b,
        Value::Status(s) => s.into_bytes(),
        Value::Verbatim { text, .. } => text.into_bytes(),
        Value::Int(n) => n.to_string().into_bytes(),
        _ => Vec::new(),
    }
}

fn message(items: Vec<Value>, pattern: bool) -> Message {
    let mut it = items.into_iter().skip(1);
    let pattern = if pattern { it.next().map(bytes) } else { None };
    Message {
        pattern,
        channel: it.next().map(bytes).unwrap_or_default(),
        payload: it.next().map(bytes).unwrap_or_default(),
    }
}

impl Drop for PubSub {
    fn drop(&mut self) {
        self.stream.shutdown(std::net::Shutdown::Both);
    }
}

impl std::fmt::Debug for PubSub {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PubSub")
            .field("server", &self.config.target())
            .field("pending", &self.pending.len())
            .finish()
    }
}
