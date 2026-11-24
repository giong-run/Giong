//! A line protocol over standard input and output, the way a local tool
//! server (such as an MCP server over stdio) talks to the program that
//! started it.
//!
//! Each line read from stdin is a command: `run PROGRAM ARGS...` runs a
//! program and answers with what it printed and its exit code; any other
//! line is echoed back. `SIGHUP` is noted on stderr instead of ending the
//! process. End of input ends it.
//!
//! ```text
//! cargo run --example stdio
//! run uname -s
//! hello
//! ```

#[cfg(target_os = "linux")]
use std::process::Stdio;

#[cfg(target_os = "linux")]
use giong::process::{self, Command, PipeWriter};
#[cfg(target_os = "linux")]
use giong::signal::{self, SignalKind};

/// Lines longer than this are refused rather than buffered without end.
#[cfg(target_os = "linux")]
const MAX_LINE: usize = 64 * 1024;

#[cfg(target_os = "linux")]
async fn answer(out: &PipeWriter, line: &str) -> std::io::Result<()> {
    let reply = match line.strip_prefix("run ") {
        Some(command) => {
            let mut words = command.split_whitespace();
            match words.next() {
                None => "error: run what?\n".to_string(),
                Some(program) => match Command::new(program)
                    .args(words)
                    .stdin(Stdio::null())
                    .kill_on_drop(true)
                    .output()
                    .await
                {
                    Ok(output) => format!(
                        "{}exit: {}\n",
                        String::from_utf8_lossy(&output.stdout),
                        output.status
                    ),
                    Err(e) => format!("error: {e}\n"),
                },
            }
        }
        None => format!("echo: {line}\n"),
    };
    out.write_all(reply.into_bytes()).await.0
}

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("giong::process is Linux only for now");
}

#[cfg(target_os = "linux")]
fn main() -> std::io::Result<()> {
    giong::block_on(async {
        let input = process::stdin()?;
        let output = process::stdout()?;

        let mut hangups = signal::listen(SignalKind::HANGUP)?;
        giong::spawn(async move {
            loop {
                hangups.recv().await;
                if let Ok(err) = process::stderr() {
                    let _ = err.write_all(&b"SIGHUP: reloading\n"[..]).await;
                }
            }
        });

        let mut pending: Vec<u8> = Vec::new();
        let mut buf = Vec::with_capacity(8192);
        loop {
            buf.clear();
            let (res, b) = input.read(buf).await;
            buf = b;
            let n = res?;
            if n == 0 {
                return Ok(()); // end of input
            }
            pending.extend_from_slice(&buf[..n]);
            while let Some(end) = pending.iter().position(|&c| c == b'\n') {
                let line: Vec<u8> = pending.drain(..=end).collect();
                let line = String::from_utf8_lossy(&line[..end]);
                answer(&output, line.trim_end_matches('\r')).await?;
            }
            if pending.len() > MAX_LINE {
                pending.clear();
                output.write_all(&b"error: line too long\n"[..]).await.0?;
            }
        }
    })
}
