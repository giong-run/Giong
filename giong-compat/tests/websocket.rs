//! WebSocket with tokio-tungstenite over `Compat`: both ends on giong, no
//! Tokio runtime.

mod common;

use common::*;
use futures_util::{SinkExt, StreamExt};
use giong::time::timeout;
use giong_compat::Compat;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
use tokio_tungstenite::tungstenite::protocol::CloseFrame;
use tokio_tungstenite::tungstenite::Message;

#[test]
fn a_websocket_echo_server_and_client() {
    giong::block_on(async {
        let (l, addr) = listener();
        let server = giong::spawn(async move {
            let tcp = l.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(Compat::new(tcp))
                .await
                .unwrap();
            let mut echoed = 0;
            while let Some(msg) = ws.next().await {
                // tungstenite answers pings and close frames by itself.
                if let msg @ (Message::Text(_) | Message::Binary(_)) = msg.unwrap() {
                    ws.send(msg).await.unwrap();
                    echoed += 1;
                }
            }
            echoed
        });

        let tcp = giong::net::TcpStream::connect(addr).await.unwrap();
        let url = format!("ws://{addr}/echo");
        let (mut ws, response) = timeout(
            LIMIT,
            tokio_tungstenite::client_async(url, Compat::new(tcp)),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(response.status(), 101);

        ws.send(Message::text("hello")).await.unwrap();
        assert_eq!(ws.next().await.unwrap().unwrap(), Message::text("hello"));

        // A large binary message, split into many frames' worth of reads.
        let big = pattern(4 << 20);
        ws.send(Message::binary(big.clone())).await.unwrap();
        match timeout(LIMIT, ws.next()).await.unwrap().unwrap().unwrap() {
            Message::Binary(back) => assert!(back[..] == big[..]),
            other => panic!("expected the binary message, got {other:?}"),
        }

        // Many small messages sent back to back before reading any.
        for i in 0..1000 {
            ws.feed(Message::text(format!("message {i}")))
                .await
                .unwrap();
        }
        ws.flush().await.unwrap();
        for i in 0..1000 {
            let msg = timeout(LIMIT, ws.next()).await.unwrap().unwrap().unwrap();
            assert_eq!(msg, Message::text(format!("message {i}")));
        }

        ws.send(Message::Ping(b"are you there"[..].into()))
            .await
            .unwrap();
        assert_eq!(
            timeout(LIMIT, ws.next()).await.unwrap().unwrap().unwrap(),
            Message::Pong(b"are you there"[..].into())
        );

        ws.close(Some(CloseFrame {
            code: CloseCode::Normal,
            reason: "done".into(),
        }))
        .await
        .unwrap();
        // The server's close frame comes back, then the stream ends.
        while let Some(msg) = timeout(LIMIT, ws.next()).await.unwrap() {
            match msg {
                Ok(Message::Close(_)) | Err(_) => {}
                Ok(other) => panic!("unexpected {other:?}"),
            }
        }
        assert_eq!(server.await.unwrap(), 1002);
    });
}
