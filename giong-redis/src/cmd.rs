//! Commands ([`Cmd`]) and pipelines ([`Pipeline`]), encoded as they are
//! built.

use crate::error::Error;
use crate::value::ToArg;

/// A command and its arguments, ready to send.
///
/// ```
/// use giong_redis::cmd;
///
/// let set = cmd("SET").arg("session:7").arg("data").arg("EX").arg(3600);
/// let mget = cmd("MGET").args(["a", "b", "c"]);
/// ```
///
/// Arguments are encoded as they are added, so sending a command copies
/// its bytes once. Build it once and send it as often as needed.
#[derive(Clone, Debug)]
pub struct Cmd {
    /// The arguments, each as a RESP bulk string.
    body: Vec<u8>,
    count: usize,
    /// The command's name in capitals.
    name: Name,
}

/// A command's name in capitals, kept inline when it is short, as every
/// Redis command's is: building a command allocates only its arguments.
#[derive(Clone)]
enum Name {
    Short { len: u8, bytes: [u8; 22] },
    Long(Box<str>),
}

impl Name {
    fn new(name: &str) -> Self {
        if name.len() <= 22 {
            let mut bytes = [0u8; 22];
            bytes[..name.len()].copy_from_slice(name.as_bytes());
            bytes[..name.len()].make_ascii_uppercase();
            Name::Short {
                len: name.len() as u8,
                bytes,
            }
        } else {
            Name::Long(name.to_ascii_uppercase().into())
        }
    }

    fn as_str(&self) -> &str {
        match self {
            // Capitalising ASCII letters keeps a str valid UTF-8.
            Name::Short { len, bytes } => {
                std::str::from_utf8(&bytes[..usize::from(*len)]).unwrap_or_default()
            }
            Name::Long(s) => s,
        }
    }
}

impl std::fmt::Debug for Name {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.as_str().fmt(f)
    }
}

/// Writes one RESP bulk string.
fn bulk(out: &mut Vec<u8>, b: &[u8]) {
    out.push(b'$');
    let mut digits = [0u8; 20];
    out.extend_from_slice(crate::value::decimal_u64(&mut digits, b.len() as u64));
    out.extend_from_slice(b"\r\n");
    out.extend_from_slice(b);
    out.extend_from_slice(b"\r\n");
}

/// Writes a command the crate builds itself (the command methods), straight
/// into the connection's buffer, with nothing allocated on the way.
pub(crate) fn encode_raw(out: &mut Vec<u8>, name: &str, args: &[&dyn ToArg]) {
    out.push(b'*');
    let mut digits = [0u8; 20];
    out.extend_from_slice(crate::value::decimal_u64(
        &mut digits,
        1 + args.len() as u64,
    ));
    out.extend_from_slice(b"\r\n");
    bulk(out, name.as_bytes());
    for a in args {
        a.with_bytes(&mut |b| bulk(out, b));
    }
}

/// Starts a command: `cmd("GET").arg("key")`.
pub fn cmd(name: &str) -> Cmd {
    Cmd::new(name)
}

impl Cmd {
    /// Starts a command; the same as [`cmd`].
    pub fn new(name: &str) -> Self {
        let mut c = Self {
            body: Vec::with_capacity(64),
            count: 0,
            name: Name::new(name),
        };
        c.push(name.as_bytes());
        c
    }

    /// Adds an argument.
    pub fn arg(mut self, arg: impl ToArg) -> Self {
        arg.with_bytes(&mut |b| self.push(b));
        self
    }

    /// Adds every argument of an iterator.
    pub fn args<I>(mut self, args: I) -> Self
    where
        I: IntoIterator,
        I::Item: ToArg,
    {
        for a in args {
            a.with_bytes(&mut |b| self.push(b));
        }
        self
    }

    fn push(&mut self, b: &[u8]) {
        bulk(&mut self.body, b);
        self.count += 1;
    }

    /// The command's name, in capitals.
    pub fn name(&self) -> &str {
        self.name.as_str()
    }

    /// Writes the command as a RESP array.
    pub(crate) fn encode(&self, out: &mut Vec<u8>) {
        out.push(b'*');
        let mut digits = [0u8; 20];
        out.extend_from_slice(crate::value::decimal_u64(&mut digits, self.count as u64));
        out.extend_from_slice(b"\r\n");
        out.extend_from_slice(&self.body);
    }

    /// The arguments after the name.
    fn rest(&self) -> impl Iterator<Item = &[u8]> {
        let mut pos = 0;
        let body = &self.body;
        std::iter::from_fn(move || {
            if pos >= body.len() {
                return None;
            }
            let end = pos + body[pos..].iter().position(|&b| b == b'\r')?;
            let len: usize = std::str::from_utf8(&body[pos + 1..end])
                .ok()?
                .parse()
                .ok()?;
            let start = end + 2;
            pos = start + len + 2;
            Some(&body[start..start + len])
        })
        .skip(1)
    }

    /// Whether the command blocks the connection until it returns.
    fn is_blocking(&self) -> bool {
        match self.name() {
            "BLPOP" | "BRPOP" | "BRPOPLPUSH" | "BLMOVE" | "BLMPOP" | "BZPOPMIN" | "BZPOPMAX"
            | "BZMPOP" | "WAIT" | "WAITAOF" => true,
            // Blocking with the BLOCK option, which comes before STREAMS.
            "XREAD" | "XREADGROUP" => self
                .rest()
                .take_while(|a| !a.eq_ignore_ascii_case(b"STREAMS"))
                .any(|a| a.eq_ignore_ascii_case(b"BLOCK")),
            _ => false,
        }
    }

    /// Refuses commands that would change the connection for other tasks or
    /// break the order of replies. `blocking` says whether blocking
    /// commands may be sent; `in_transaction` that it is queued inside
    /// `MULTI`, where blocking commands do not block.
    pub(crate) fn check(&self, blocking: bool, in_transaction: bool) -> Result<(), Error> {
        let name = self.name();
        let why = match name {
            "MULTI" | "EXEC" | "DISCARD" => {
                "MULTI, EXEC and DISCARD are not sent one by one, since other tasks may send \
                 commands on the same connection in between; put the commands in a pipeline \
                 made atomic (Pipeline::new().atomic()), which wraps them in MULTI and EXEC"
            }
            "WATCH" | "UNWATCH" => {
                "WATCH is sent with Client::watch, on a connection the task has to itself \
                 (Pool::take), which gives a Watch to run the transaction with"
            }
            "SELECT" => {
                "the database is chosen when connecting, for every connection: \
                 Config::database, or /N at the end of the URL"
            }
            "AUTH" | "HELLO" => {
                "the connection logs in when it opens, with RESP3: set Config::user and \
                 Config::password (or user:password@ in the URL)"
            }
            "SUBSCRIBE" | "PSUBSCRIBE" | "SSUBSCRIBE" | "UNSUBSCRIBE" | "PUNSUBSCRIBE"
            | "SUNSUBSCRIBE" => {
                "a subscribed connection only receives messages; open one with \
                 PubSub::connect"
            }
            "MONITOR" | "SYNC" | "PSYNC" | "REPLCONF" => {
                "it turns the connection into a stream for replication or monitoring, which \
                 giong-redis does not read; use redis-cli for it"
            }
            "QUIT" | "RESET" => "drop the Client (or the pool) to close its connections",
            "CLIENT"
                if self
                    .rest()
                    .next()
                    .is_some_and(|a| a.eq_ignore_ascii_case(b"REPLY")) =>
            {
                "CLIENT REPLY OFF or SKIP stops the server answering, and the replies of other \
                 tasks' commands would be matched to the wrong commands"
            }
            _ if self.is_blocking() && !blocking && !in_transaction => {
                return Err(Error::command(format!(
                    "{name} blocks the connection until it returns, holding up every command \
                     other tasks send on it; send it with Client::query_blocking on a \
                     connection the task has to itself (Pool::take)"
                )))
            }
            _ => return Ok(()),
        };
        let shown = if name == "CLIENT" {
            "CLIENT REPLY".to_owned()
        } else {
            name.to_owned()
        };
        Err(Error::command(format!("{shown} cannot be sent: {why}")))
    }
}

/// Several commands sent together, read back together: one round trip for
/// all of them.
///
/// ```
/// use giong_redis::{cmd, Pipeline};
///
/// let pipe = Pipeline::new()
///     .add(cmd("INCR").arg("visits"))
///     .add(cmd("EXPIRE").arg("visits").arg(60));
/// let transfer = Pipeline::new()
///     .add(cmd("DECRBY").arg("balance:1").arg(10))
///     .add(cmd("INCRBY").arg("balance:2").arg(10))
///     .atomic();
/// ```
///
/// Its result is an array with one reply per command, read as a tuple
/// (`(i64, bool)`), a `Vec`, or a `Vec<Value>` to see each command's error
/// apart.
#[derive(Clone, Debug, Default)]
pub struct Pipeline {
    cmds: Vec<Cmd>,
    atomic: bool,
}

impl Pipeline {
    /// An empty pipeline.
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds a command.
    // A builder step, not addition: `std::ops::Add` would not fit.
    #[allow(clippy::should_implement_trait)]
    pub fn add(mut self, cmd: Cmd) -> Self {
        self.cmds.push(cmd);
        self
    }

    /// Makes it a transaction: the commands are sent between `MULTI` and
    /// `EXEC`, so the server runs them one after the other with no command
    /// of another client in between. The result is `EXEC`'s array.
    ///
    /// If the server refuses a command as it is queued (a wrong number of
    /// arguments), none run and the result is that error. A command that
    /// fails as it runs (`WRONGTYPE`) does not stop the others; its error
    /// is in the array.
    pub fn atomic(mut self) -> Self {
        self.atomic = true;
        self
    }

    /// How many commands it holds.
    pub fn len(&self) -> usize {
        self.cmds.len()
    }

    /// Whether it holds no command.
    pub fn is_empty(&self) -> bool {
        self.cmds.is_empty()
    }

    pub(crate) fn is_atomic(&self) -> bool {
        self.atomic
    }

    /// Checks every command (see [`Cmd::check`]).
    pub(crate) fn check(&self, atomic: bool) -> Result<(), Error> {
        for (i, c) in self.cmds.iter().enumerate() {
            c.check(false, atomic).map_err(|e| {
                Error::command(format!(
                    "command {} of the pipeline ({}): {}",
                    i + 1,
                    c.name(),
                    e.to_string()
                        .split_once(": ")
                        .map_or("", |(_, m)| m)
                        .rsplit_once(" See ")
                        .map_or("", |(m, _)| m)
                ))
            })?;
        }
        Ok(())
    }

    /// Writes the commands, within `MULTI` and `EXEC` when `atomic`.
    pub(crate) fn encode(&self, out: &mut Vec<u8>, atomic: bool) {
        if atomic {
            out.extend_from_slice(b"*1\r\n$5\r\nMULTI\r\n");
        }
        for c in &self.cmds {
            c.encode(out);
        }
        if atomic {
            out.extend_from_slice(b"*1\r\n$4\r\nEXEC\r\n");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wire(c: &Cmd) -> String {
        let mut out = Vec::new();
        c.encode(&mut out);
        String::from_utf8(out).unwrap()
    }

    #[test]
    fn commands_are_encoded_as_arrays_of_bulk_strings() {
        assert_eq!(wire(&cmd("PING")), "*1\r\n$4\r\nPING\r\n");
        assert_eq!(
            wire(&cmd("set").arg("k").arg(-5).arg(b"\r\n".as_slice())),
            "*4\r\n$3\r\nset\r\n$1\r\nk\r\n$2\r\n-5\r\n$2\r\n\r\n\r\n"
        );
        assert_eq!(
            wire(&cmd("DEL").args(["a", "bc"])),
            "*3\r\n$3\r\nDEL\r\n$1\r\na\r\n$2\r\nbc\r\n"
        );
        assert_eq!(cmd("get").name(), "GET");
        let long = "x".repeat(1000);
        assert!(wire(&cmd("SET").arg("k").arg(&long)).contains("$1000\r\n"));
    }

    #[test]
    fn commands_that_would_disturb_other_tasks_are_refused() {
        for name in [
            "MULTI",
            "exec",
            "DISCARD",
            "WATCH",
            "UNWATCH",
            "SELECT",
            "AUTH",
            "HELLO",
            "SUBSCRIBE",
            "psubscribe",
            "MONITOR",
            "QUIT",
            "RESET",
        ] {
            let e = cmd(name).arg("x").check(true, false).unwrap_err();
            assert_eq!(e.kind(), crate::ErrorKind::Command, "{name}");
        }
        assert!(cmd("CLIENT")
            .arg("reply")
            .arg("OFF")
            .check(true, false)
            .is_err());
        assert!(cmd("CLIENT")
            .arg("SETNAME")
            .arg("x")
            .check(false, false)
            .is_ok());
        assert!(cmd("GET").arg("k").check(false, false).is_ok());
    }

    #[test]
    fn blocking_commands_need_their_own_connection() {
        let blpop = cmd("BLPOP").arg("q").arg(0);
        let e = blpop.check(false, false).unwrap_err();
        assert!(e.to_string().contains("query_blocking"), "{e}");
        assert!(blpop.check(true, false).is_ok());
        assert!(blpop.check(false, true).is_ok());
        let xread = cmd("XREAD").args(["COUNT", "1", "BLOCK", "100", "STREAMS", "s", "0"]);
        assert!(xread.check(false, false).is_err());
        // A stream named "block" does not make XREAD block.
        let named = cmd("XREAD").args(["STREAMS", "block", "0"]);
        assert!(named.check(false, false).is_ok());
    }

    #[test]
    fn a_pipeline_names_the_command_it_refuses() {
        let pipe = Pipeline::new()
            .add(cmd("SET").arg("a").arg(1))
            .add(cmd("BLPOP").arg("q").arg(1));
        let e = pipe.check(false).unwrap_err();
        assert!(
            e.to_string().contains("command 2 of the pipeline (BLPOP)"),
            "{e}"
        );
        assert!(pipe.check(true).is_ok());
        let mut out = Vec::new();
        pipe.encode(&mut out, true);
        let text = String::from_utf8(out).unwrap();
        assert!(text.starts_with("*1\r\n$5\r\nMULTI\r\n"));
        assert!(text.ends_with("*1\r\n$4\r\nEXEC\r\n"));
    }
}
