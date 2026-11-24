use std::time::Duration;

const VERSION: &str = "v0";

fn main() {
    let listen = std::env::var("LISTEN").unwrap();
    let workers: usize = std::env::var("WORKERS").ok().and_then(|w| w.parse().ok()).unwrap_or(1);
    eprintln!("starting {VERSION} with {workers} workers");
    giong::Builder::new()
        .workers(workers)
        .shutdown_on_signals(true)
        .shutdown_grace(Duration::from_secs(5))
        .build()
        .unwrap()
        .run(move |_| {
            let listen = listen.clone();
            async move {
                let l = giong::net::TcpListener::bind(listen.as_str()).unwrap();
                while let Some(s) = giong::until_shutdown(l.accept()).await {
                    let s = s.unwrap();
                    giong::spawn(async move {
                        let buf = vec![0u8; 1024];
                        let (n, _buf) = s.read(buf).await;
                        if n.unwrap_or(0) == 0 { return; }
                        giong::time::sleep(Duration::from_millis(20)).await;
                        let body = format!("HTTP/1.1 200 OK\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{VERSION}", VERSION.len());
                        let _ = s.write_all(body.into_bytes()).await;
                    });
                }
            }
        })
        .unwrap();
}
