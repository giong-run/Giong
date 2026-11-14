//! Framing with tokio-util's codecs over `Compat`: the usual way to put a
//! custom protocol on a byte stream.

mod common;

use bytes::Bytes;
use common::*;
use futures_util::{SinkExt, StreamExt};
use giong::net::UnixStream;
use giong::time::timeout;
use giong_compat::Compat;
use tokio_util::codec::{Framed, LengthDelimitedCodec, LinesCodec};

#[test]
fn length_delimited_frames_over_a_unix_socket() {
    giong::block_on(async {
        let (a, b) = UnixStream::pair().unwrap();
        let a = Framed::new(Compat::new(a), LengthDelimitedCodec::new());
        let mut b = Framed::new(Compat::new(b), LengthDelimitedCodec::new());
        let peer = giong::spawn(async move {
            // Echo every frame back, reversed.
            while let Some(frame) = b.next().await {
                let mut frame = frame.unwrap().to_vec();
                frame.reverse();
                b.send(Bytes::from(frame)).await.unwrap();
            }
        });
        // Frames from empty to a megabyte, written by one task and read by
        // another (both ends would block on full buffers otherwise).
        let frames: Vec<Vec<u8>> = (0..200usize).map(|i| pattern(i * i * 25)).collect();
        let (mut sink, mut stream) = a.split();
        let to_send = frames.clone();
        let writer = giong::spawn(async move {
            for f in to_send {
                sink.feed(Bytes::from(f)).await.unwrap();
            }
            sink.close().await.unwrap();
        });
        for f in &frames {
            let back = timeout(LIMIT, stream.next())
                .await
                .unwrap()
                .unwrap()
                .unwrap();
            let mut expected = f.clone();
            expected.reverse();
            assert!(back[..] == expected[..], "a frame of {} bytes", f.len());
        }
        timeout(LIMIT, writer).await.unwrap().unwrap();
        assert!(timeout(LIMIT, stream.next()).await.unwrap().is_none());
        timeout(LIMIT, peer).await.unwrap().unwrap();
    });
}

#[test]
fn a_line_protocol_over_tcp() {
    giong::block_on(async {
        let (c, s) = pair().await;
        let server = giong::spawn(async move {
            let mut lines = Framed::new(Compat::new(s), LinesCodec::new_with_max_length(1024));
            while let Some(line) = lines.next().await {
                let line = line.unwrap();
                let reply = match line.split_once(' ') {
                    Some(("ADD", args)) => args
                        .split(' ')
                        .map(|n| n.parse::<i64>().unwrap())
                        .sum::<i64>()
                        .to_string(),
                    Some(("ECHO", text)) => text.to_owned(),
                    _ => "ERR".to_owned(),
                };
                lines.send(reply).await.unwrap();
            }
        });
        let mut lines = Framed::new(Compat::new(c), LinesCodec::new());
        lines.send("ADD 1 2 3 4").await.unwrap();
        assert_eq!(lines.next().await.unwrap().unwrap(), "10");
        lines.send("ECHO giong").await.unwrap();
        assert_eq!(lines.next().await.unwrap().unwrap(), "giong");
        lines.send("NOPE").await.unwrap();
        assert_eq!(lines.next().await.unwrap().unwrap(), "ERR");
        drop(lines);
        timeout(LIMIT, server).await.unwrap().unwrap();
    });
}
