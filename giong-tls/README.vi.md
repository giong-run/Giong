# giong-tls

TLS cho runtime [giong](../README.vi.md), xây trên [rustls](https://github.com/rustls/rustls) (TLS 1.2 và 1.3, mật mã của `ring`, không cần OpenSSL): `TlsAcceptor` cho server, `TlsConnector` cho client.

*[English](README.md)*

```rust
use giong::net::TcpListener;
use giong_tls::TlsAcceptor;

fn main() -> std::io::Result<()> {
    let acceptor = TlsAcceptor::from_pem(
        &std::fs::read("cert.pem")?,
        &std::fs::read("key.pem")?,
    )?;

    giong::block_on(async move {
        let listener = TcpListener::bind("0.0.0.0:8443")?;
        loop {
            let tcp = listener.accept().await?;
            let acceptor = acceptor.clone();
            giong::spawn(async move {
                let Ok(tls) = acceptor.accept(tcp).await else { return };
                while let Ok(data) = tls.recv().await {
                    if data.is_empty() {
                        break; // phía kia đã đóng
                    }
                    if tls.write_all(data).await.0.is_err() {
                        break;
                    }
                }
                let _ = tls.shutdown().await;
            });
        }
    })
}
```

Muốn HTTPS thì không cần tự viết vòng lặp này: bật feature `tls` của [`giong-http`](../giong-http/README.vi.md) và gọi `Server::serve_tls`.

Một client, kiểm tra chứng chỉ của server bằng các CA đã cài trên máy:

```rust
use giong::net::TcpStream;
use giong_tls::TlsConnector;

fn main() -> std::io::Result<()> {
    let connector = TlsConnector::with_system_roots()?;

    giong::block_on(async move {
        let tcp = TcpStream::connect("example.com:443").await?;
        let tls = connector.connect("example.com", tcp).await?;
        let request = b"GET / HTTP/1.1\r\nHost: example.com\r\nConnection: close\r\n\r\n";
        tls.write_all(request.to_vec()).await.0?;
        loop {
            let data = tls.recv().await?;
            if data.is_empty() {
                break;
            }
            print!("{}", String::from_utf8_lossy(&data));
        }
        Ok(())
    })
}
```

## API

| | |
|---|---|
| `TlsAcceptor::from_pem(cert, key)` | Một chuỗi chứng chỉ và khoá riêng ở dạng PEM. TLS 1.2 và 1.3, bộ mã mặc định của rustls, không xác thực client, ALPN `http/1.1`. |
| `TlsAcceptor::new(config)` | Dùng `rustls::ServerConfig` tự cấu hình (nhiều chứng chỉ theo SNI, xác thực client, ALPN khác). Crate xuất lại `rustls` để dùng đúng phiên bản. |
| `.handshake_timeout(thời gian)` | Thời gian tối đa cho một handshake, mặc định 10 s. |
| `acceptor.accept(tcp)` | Thực hiện handshake, trả về `TlsStream`. |
| `TlsConnector::with_system_roots()` | Tin các CA đã cài trên máy: `SSL_CERT_FILE` / `SSL_CERT_DIR` nếu có, không thì bundle của bản phân phối (`/etc/ssl/certs/ca-certificates.crt` và các chỗ quen thuộc khác). Báo lỗi `NotFound` rõ ràng nếu máy không có, như image `FROM scratch`. |
| `TlsConnector::with_webpki_roots()` | Bộ chứng chỉ gốc của Mozilla biên dịch sẵn vào chương trình, cho máy không có bundle. Cần feature `webpki-roots`. |
| `TlsConnector::with_root_pem(pem)`, `with_roots(store)` | Chỉ tin các CA được đưa vào: CA nội bộ, hoặc chứng chỉ tự ký của chính server. |
| `TlsConnector::new(config)` | Dùng `rustls::ClientConfig` tự cấu hình. |
| `.alpn(&[b"h2", b"http/1.1"])`, `.client_auth(cert, key)` | Giao thức đề nghị qua ALPN; chứng chỉ client cho server đòi xác thực hai chiều (mTLS). |
| `connector.connect(tên, tcp)` | Thực hiện handshake, kiểm tra chứng chỉ có hợp lệ cho `tên` không (tên DNS, gửi qua SNI, hoặc địa chỉ IP). Chứng chỉ cho tên khác, hết hạn hay từ CA lạ đều thất bại với `InvalidData` và câu báo rõ lý do. Các bản clone của một connector dùng chung cache phiên, nên kết nối lại sẽ nối tiếp phiên cũ. |
| `tls.recv()` | Dữ liệu đã giải mã. Buffer rỗng nghĩa là phía kia đã đóng. |
| `tls.write_all(buf)` | Mã hoá và gửi hết `buf`, rồi trả lại buffer, giống `TcpStream::write_all`. |
| `tls.shutdown()` | Gửi `close_notify` rồi đóng chiều gửi. |
| `tls.alpn_protocol()`, `tls.tcp()` | Giao thức đã thoả thuận qua ALPN; socket TCP bên dưới. |
| `tls.peer_certificates()`, `tls.resumed()` | Chuỗi chứng chỉ phía kia đưa ra; handshake có nối tiếp phiên cũ không. |

Bốn điều cần nhớ:

1. `recv()` an toàn khi bị huỷ: bọc trong `timeout` không làm mất dữ liệu.
2. `write_all()` thì không. Nếu future bị huỷ giữa chừng một bản ghi TLS, luồng bản ghi đã bị cắt ngang, nên mọi lần gửi sau đó trả về lỗi thay vì gửi tiếp dữ liệu hỏng. Dữ liệu phía kia đã gửi vẫn nhận được.
3. Một task nhận trong khi task khác gửi trên cùng một `TlsStream` là được; các bản ghi không bị trộn vào nhau.
4. Dữ liệu nhận được nguyên vẹn luôn được trả về trước lỗi xảy ra sau nó.

## Hiệu năng

Đo bằng `bench/tls.sh` trên một core, cùng rustls và `ring` ở cả hai bên: HTTPS qua `giong-http` đạt 144,8k req/s ở `/json` so với 81,0k của Axum + rustls; so với HTTP thường trên giong, TLS tốn 4% ở `/json` và 14% với response 8 KB. Số handshake mỗi giây ngang nhau (7,1k so với 7,3k), vì phần đó gần như chỉ là mật mã. Chi tiết và cách đọc ở [README chính (tiếng Anh)](../README.md#https).

## Chưa có

kTLS, tải lại chứng chỉ khi đang chạy (tự làm được qua `ServerConfig::cert_resolver`), kiểm tra thu hồi chứng chỉ phía client (rustls làm được, qua `TlsConnector::new` với verifier riêng). Crate chưa được kiểm toán bảo mật độc lập; phần mật mã và giao thức là của rustls.

## Kiểm thử

`cargo test -p giong-tls --all-features`: 7 test phía server trên socket thật với client rustls (TLS 1.2 và 1.3, 4 MB mỗi chiều, ALPN, `recv` bị huỷ liên tục, hai task cùng gửi, ghi bị huỷ giữa chừng, handshake hỏng hoặc treo); 14 test phía client với server của giong và một server rustls chạy chặn (TLS 1.2 và 1.3 với 4 MB mỗi chiều, chứng chỉ cho tên khác, CA lạ, địa chỉ IP làm tên, ALPN, mTLS có và không có chứng chỉ client, nối tiếp phiên, server không trả lời hoặc ngắt giữa handshake); 6 unit test cho việc tìm chứng chỉ của máy và đọc tên server; 3 ví dụ trong tài liệu. Chín test HTTPS đầu-cuối nằm trong `giong-http` (`cargo test -p giong-http --features tls`).
