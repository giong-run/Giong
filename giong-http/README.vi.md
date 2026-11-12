# giong-http

Server HTTP/1.1 nhỏ và nghiêm ngặt cho runtime [giong](../README.vi.md).

*[English](README.md)*

```rust
use std::time::Duration;
use giong::net::TcpListener;
use giong_http::{Request, Response, Server};

async fn handler(req: Request) -> Response {
    match (req.method(), req.path()) {
        ("GET", "/health") => Response::text("ok"),
        ("POST", "/echo") => Response::new(200).body(req.body().to_vec()),
        _ => Response::new(404),
    }
}

fn main() -> std::io::Result<()> {
    giong::Builder::new()
        .shutdown_on_signals(true)
        .shutdown_grace(Duration::from_secs(10))
        .build()?
        .run(|_worker| async {
            // Mỗi worker một listener; kernel chia kết nối cho các core.
            let listener = TcpListener::bind("0.0.0.0:8080").expect("bind");
            Server::new().serve(listener, handler).await;
        })
}
```

## Có gì

- HTTP/1.1 và HTTP/1.0, keep-alive, pipelining. Response của các request pipeline được gửi chung một lần ghi; nếu một handler phải chờ thì các response trước nó được gửi đi trước.
- Body theo `Content-Length` hoặc chunked, nhận đủ rồi mới gọi handler. Hỗ trợ `Expect: 100-continue`.
- `Request` dùng chung buffer nhận của kết nối thay vì chép ra, nên tạo một request không cấp phát bộ nhớ. Handler giữ request lại bao lâu cũng được.
- Giới hạn kích thước head (64 KiB) và body (1 MiB); timeout cho kết nối rảnh (60 s), request chậm (30 s, chống slowloris) và client đọc chậm (30 s). Tất cả đều chỉnh được trên `Server`.
- Giới hạn số kết nối mở tùy chọn (`Server::max_connections`): khi chạm giới hạn, server ngừng nhận kết nối mới, client mới chờ trong hàng đợi listen của kernel cho đến khi có kết nối đóng.
- `Request::peer_addr` trả về địa chỉ của client (`None` khi chạy qua Unix socket).
- Tắt êm: request đang xử lý được trả lời kèm `Connection: close`, kết nối rảnh đóng ngay.
- HTTPS qua feature `tls` (xem dưới).
- Phục vụ qua Unix socket bằng `Server::serve_unix`, cách thường dùng khi đứng sau một reverse proxy trên cùng máy.
- Tự thêm `Date` và `Content-Length`. Giá trị header chứa ký tự điều khiển bị từ chối (chống response splitting).

## Parse nghiêm ngặt

Request mà một proxy đứng trước có thể hiểu khác đi đều bị trả `400`, vì đó chính là cách tấn công request smuggling hoạt động:

- hai `Content-Length` khác nhau, hoặc `Content-Length` cùng với `Transfer-Encoding`
- khoảng trắng trước dấu hai chấm, header gập dòng, xuống dòng chỉ có LF
- ký tự điều khiển trong tên hoặc giá trị header, thiếu hoặc lặp `Host` ở HTTP/1.1

Các mã lỗi khác: `408` request quá chậm, `413` body quá lớn, `417` `Expect` lạ, `431` head quá lớn, `501` transfer-encoding không phải chunked, `505` không phải HTTP/1.x.

## HTTPS

Bật feature `tls` để có `Server::serve_tls`, dùng crate [`giong-tls`](../giong-tls/README.vi.md) (rustls, TLS 1.2 và 1.3):

```toml
giong-http = { version = "0.2", features = ["tls"] }
giong-tls = "0.2"
```

```rust
let acceptor = giong_tls::TlsAcceptor::from_pem(
    &std::fs::read("cert.pem")?,
    &std::fs::read("key.pem")?,
)?;
// ... trong mỗi worker:
let listener = TcpListener::bind("0.0.0.0:8443").expect("bind");
Server::new().serve_tls(listener, acceptor.clone(), handler).await;
```

Handler, giới hạn, timeout và tắt êm giống hệt bản HTTP thường. Handshake chạy trong task riêng của từng kết nối nên một client chậm không chặn việc nhận kết nối mới; client không nói TLS (quét cổng, HTTP thường gửi nhầm cổng) bị đóng sau thời gian chờ handshake. Khi đóng kết nối, server gửi `close_notify` để client phân biệt được response trọn vẹn với kết nối bị cắt.

Muốn chạy trên một kiểu kết nối khác (một tầng TLS khác, kết nối giả trong test), cài trait `Transport` rồi gọi `Server::serve_connection`.

## Chưa có

HTTP/2, body dạng stream (body lớn hơn giới hạn bị từ chối chứ không được truyền dần), nén, routing.

## Kiểm thử

`cargo test -p giong-http --all-features`: 15 unit test (gồm 500.000 đầu vào ngẫu nhiên cho parser và bộ giải mã chunked), 19 test đầu-cuối trên socket thật (một trong số đó qua Unix socket), 11 test HTTPS đầu-cuối với client rustls (TLS 1.2 và 1.3, pipelining, body 3 MB, lỗi 400/413, kết nối rảnh, client không nói TLS, tắt êm, 16 thread client cùng lúc), 5 ví dụ trong tài liệu.
