<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="https://raw.githubusercontent.com/giong-run/Giong/main/assets/giong-logo-dark.svg">
    <img alt="giong" src="https://raw.githubusercontent.com/giong-run/Giong/main/assets/giong-logo-light.svg" width="340">
  </picture>
</p>

<p align="center"><b>Async runtime cho Linux theo mô hình thread-per-core, xây thẳng trên io_uring.</b></p>

<p align="center">
  <a href="https://github.com/giong-run/Giong/actions/workflows/ci.yml"><img alt="CI" src="https://github.com/giong-run/Giong/actions/workflows/ci.yml/badge.svg"></a>
  <img alt="License: MIT OR Apache-2.0" src="https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue">
  <img alt="Platform: Linux 6.0+" src="https://img.shields.io/badge/platform-Linux%206.0%2B-informational">
</p>

Tên lấy từ Thánh Gióng: nằm im ba năm, cất tiếng là vươn vai thành người khổng lồ. giong
lúc rảnh gần như không tốn gì, có tải thì dùng hết mọi core.

*Bản đầy đủ (thiết kế, phương pháp đo, toàn bộ bảng số liệu) nằm ở [README.md](README.md),
viết bằng tiếng Anh. File này là bản tóm tắt.*

> **Trạng thái: 0.2.** Đủ để viết server thật: TCP, UDP, Unix socket, file, timer, nhiều
> core, tắt êm, cùng hai crate đi kèm: [`giong-http`](giong-http/README.vi.md) (server
> HTTP/1.1 nghiêm ngặt) và [`giong-tls`](giong-tls/README.vi.md) (TLS 1.2/1.3 trên rustls)
> để chạy HTTPS trực tiếp và kết nối tới dịch vụ TLS. 169 test đều qua trên cả ba cấu hình io_uring; ThreadSanitizer
> và valgrind sạch. **Chưa** chạy production, chưa có HTTP/2.

## Vì sao là giong

- **Một syscall cho cả vòng lặp, không phải cho từng thao tác.** Mọi lệnh đọc, ghi, accept,
  đóng, huỷ của một vòng đi vào kernel bằng một lần `io_uring_enter`. Mỗi request tốn
  khoảng **0,03 syscall**; server dùng epoll tốn 2.
- **Núm chỉnh độ trễ.** `Builder::spin_before_park` cho worker canh ring thay vì đi ngủ.
  Với một kết nối: **nhiều hơn 48% request và độ trễ giảm một phần ba** (19 µs thay vì
  28 µs).
- **Trụ được khi kết nối dồn lên.** Ở 5.000 kết nối, giong tốn ít CPU mỗi request nhất
  (thấp hơn runtime đứng sau 11%) và có p99 thấp nhất. Ở 1.000 kết nối ngang server epoll
  tự viết và hơn các runtime còn lại.
- **Có sẵn HTTP và HTTPS.** `giong-http` cùng `giong-tls` phục vụ HTTPS **144,8k
  request/giây trên một core, so với 81,0k của Axum** dùng cùng rustls.
- **An toàn khi huỷ thao tác.** Drop một future I/O không bao giờ giải phóng vùng nhớ kernel
  còn ghi vào; bọc `timeout` quanh `recv`, `recv_from` hay `accept` không làm mất dữ liệu
  hay kết nối.
- **Đủ nhỏ để đọc hết.** Runtime khoảng 5.400 dòng, ba thư viện phụ thuộc, mỗi khối
  `unsafe` đều có chú thích lý do.

Điều giong **không** phải: nhanh hơn tất cả ở mọi bài. Ở bài 50 kết nối quen thuộc, giong,
Monoio đã chỉnh và Tokio thuần nằm trong khoảng nhiễu của nhau, vì phần lớn chi phí là TCP
stack của kernel chứ không phải runtime.

## Kết quả chính

Mỗi server một core, qua loopback, trung vị của 5 vòng đo xen kẽ, trên máy ảo 4 vCPU.

| | giong | Monoio (đã chỉnh) | Tokio thuần | epoll tự viết | Glommio |
|---|---|---|---|---|---|
| Syscall mỗi request | 0,03 | 0,04 | 2,05 | 2,03 | 2,10 |
| 1 kết nối: request/giây | **47,9k** ¹ | 32,3k | 31,0k | 31,9k | 27,4k |
| 1 kết nối: độ trễ trung vị | **19 µs** ¹ | 28 µs | 29 µs | 29 µs | 33 µs |
| 50 kết nối: request/giây | 171,7k (204,8k ¹) | 176,1k | 169,6k | 157,2k | 137,4k |
| 50 kết nối, response 8 KB | 63,5k | 64,4k | 35,7k | 37,1k | 36,4k |
| 1.000 kết nối: CPU mỗi request | 4,39 µs | 4,48 µs | 4,74 µs | 4,40 µs | 5,66 µs |
| 1.000 kết nối: p99 | **5,9 ms** | 8,1 ms | 50,0 ms | 7,3 ms | 10,1 ms |
| 5.000 kết nối: CPU mỗi request | **4,42 µs** | 5,25 µs | 5,74 µs | 4,99 µs | 8,29 µs |
| 5.000 kết nối: p99 | **26 ms** | 44 ms | 54 ms | 32 ms | 71 ms |

¹ với `spin_before_park(200 µs)`. Không bật thì giong đạt 32,4k request/giây, 28 µs, ngang
các runtime khác.

Cách đọc cho đúng:

- "Monoio (đã chỉnh)" là Monoio bật cờ `DEFER_TASKRUN`. Cấu hình mặc định của Monoio chậm
  hơn khoảng 20% ở response nhỏ; so với bản mặc định thì giong sẽ được lợi không công bằng.
- Ở 50 kết nối, `wrk` là giới hạn chứ không phải server (core của giong chỉ bận 78%), nên
  số request/giây ở đó không xếp hạng được. Cột CPU mỗi request mới đáng tin.
- Response 8 KB: giong và Monoio gấp đôi phần còn lại vì gửi cả lô response trong một
  syscall. Phần lớn khoảng cách đến từ máy ảo; trên phần cứng thật sẽ nhỏ hơn.
- Chưa đo: hơn hai core, mạng thật, ổ đĩa thật, nhánh chính mới nhất của Monoio/Glommio.

## Yêu cầu

- **Linux, kernel 6.0 trở lên** (nên dùng 6.1+). Rust 1.80+.
- **Không chạy trên Windows hay macOS**, vì io_uring chỉ có trên Linux. Trên Windows:
  - WSL2 với kernel 6.x (`uname -r`; nếu còn 5.15 thì chạy `wsl --update`).
  - Máy ảo hoặc server Linux.
  - Docker, thêm `--security-opt seccomp=unconfined` vì cấu hình mặc định chặn io_uring.

## Chạy thử

```bash
git clone https://github.com/giong-run/Giong.git && cd Giong
cargo test --workspace --all-features   # toàn bộ test của ba crate
cargo run --release --example echo      # echo server, cổng 7000
cargo run --release --example http      # HTTP server tự viết, cổng 3006
bench/run.sh                            # thông lượng, so với 9 server khác
bench/scale.sh                          # từ 1 đến 5.000 kết nối
```

Một dịch vụ HTTP chạy trên mọi core, dùng `giong-http`:

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
        .shutdown_on_signals(true)                  // Ctrl-C / SIGTERM → dừng êm
        .shutdown_grace(Duration::from_secs(10))    // request đang dở vẫn được trả lời
        .build()?
        .run(|_worker| async {
            let listener = TcpListener::bind("0.0.0.0:8080").expect("bind");
            Server::new().serve(listener, handler).await;
        })
}
```

Muốn HTTPS thì bật feature `tls` và gọi `serve_tls` với một `giong_tls::TlsAcceptor`.

## Có gì trong hộp

| Nhóm | API |
|---|---|
| Khởi chạy | `block_on`, `Builder` (`workers`, `pin_cores`, `spin_before_park`, `recv_buffers`, `shutdown_on_signals`, `shutdown_grace`) |
| Task | `spawn`, `JoinHandle::abort`, `yield_now`; future không cần `Send` |
| TCP | `TcpListener`, `TcpStream` (`recv`, `read`, `write`, `write_all`) |
| Unix socket | `UnixListener`, `UnixStream` (giống `TcpStream`, thêm `pair`) |
| UDP | `UdpSocket` (`recv_from`, `recv` dùng buffer của pool; `read_from`, `read` dùng buffer riêng; `send_to`, `send`) |
| File | `fs::File` (`read_at`, `write_at`, `write_all_at`, `sync_all`, `metadata`, `close`), `fs::read`, `fs::write`, `rename`, `remove_file`, `create_dir` |
| Thời gian | `sleep`, `sleep_until`, `timeout` |
| Tắt | `shutdown_signal`, `until_shutdown`, `is_shutting_down`, `Handle::shutdown` |
| Chéo thread | `Handle::spawn_on`, `sync::mpsc` |
| HTTP, TLS | [`giong-http`](giong-http/README.vi.md), [`giong-tls`](giong-tls/README.vi.md) |

## Giới hạn hiện tại

- Chưa có DNS bất đồng bộ, HTTP/2, body HTTP dạng stream.
- `giong-tls` chưa được kiểm toán bảo mật độc lập; phần mật mã là của rustls.
- Không tương thích hệ sinh thái Tokio (hyper, sqlx, reqwest không chạy được).
- Task không chuyển core.
- Tắt êm phải tự bật bằng `shutdown_grace`; mặc định vẫn là cắt kết nối.
- Chưa chạy trên kernel 6.0 thật; các đường dự phòng cho kernel cũ được kiểm tra bằng cách
  ép chạy trên kernel 6.18.

## Tài liệu khác

- Mã nguồn và issue: <https://github.com/giong-run/Giong>
- [README.md](README.md): thiết kế chi tiết, benchmark đầy đủ, kiểm thử, so sánh với Monoio và Glommio.
- [CONTRIBUTING.md](CONTRIBUTING.md), [ROADMAP.md](ROADMAP.md), [CHANGELOG.md](CHANGELOG.md).

## Giấy phép

Giấy phép kép, người dùng tự chọn: Apache-2.0 ([LICENSE-APACHE](LICENSE-APACHE)) hoặc MIT
([LICENSE-MIT](LICENSE-MIT)). Mọi đóng góp gửi vào dự án được hiểu là cấp theo cùng giấy
phép kép này, trừ khi người đóng góp nói rõ khác đi.
