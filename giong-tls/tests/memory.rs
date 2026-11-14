//! Heap a connection keeps once it has sent a large write: this binary
//! counts every allocation, so it holds this test alone.

use std::alloc::{GlobalAlloc, Layout, System};
use std::rc::Rc;
use std::sync::atomic::{AtomicIsize, Ordering};
use std::time::Duration;

use giong::net::{TcpListener, TcpStream};
use giong_tls::{TlsAcceptor, TlsConnector};

struct Counting;

static LIVE: AtomicIsize = AtomicIsize::new(0);

// SAFETY: every call is passed on to the system allocator unchanged; only
// the count of live bytes is added.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        LIVE.fetch_add(layout.size() as isize, Ordering::Relaxed);
        System.alloc(layout)
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        LIVE.fetch_sub(layout.size() as isize, Ordering::Relaxed);
        System.dealloc(ptr, layout)
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        LIVE.fetch_add(size as isize - layout.size() as isize, Ordering::Relaxed);
        System.realloc(ptr, layout, size)
    }
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

/// An idle connection that once sent 1 MiB keeps no more than one that sent
/// a few bytes, give or take rustls' own buffers. It used to keep its
/// largest batch of outgoing records, about 64 KiB, until it closed.
#[test]
fn a_large_write_leaves_no_large_buffer_behind() {
    const PAIRS: usize = 50;
    let key = rcgen::generate_simple_self_signed(vec!["localhost".to_string()]).unwrap();
    let acceptor = TlsAcceptor::from_pem(
        key.cert.pem().as_bytes(),
        key.key_pair.serialize_pem().as_bytes(),
    )
    .unwrap();
    let connector = TlsConnector::with_root_pem(key.cert.pem().as_bytes()).unwrap();
    giong::block_on(async move {
        let listener = Rc::new(TcpListener::bind("127.0.0.1:0").unwrap());
        let addr = listener.local_addr().unwrap();
        let mut pairs = Vec::new();
        for _ in 0..PAIRS {
            let (acceptor, listener) = (acceptor.clone(), listener.clone());
            let accept =
                giong::spawn(
                    async move { acceptor.accept(listener.accept().await.unwrap()).await },
                );
            let tcp = TcpStream::connect(addr).await.unwrap();
            let client = connector.connect("localhost", tcp).await.unwrap();
            let server = accept.await.unwrap().unwrap();
            // Both sides past the handshake, and each buffer used once.
            server.write_all(b"hi".to_vec()).await.0.unwrap();
            assert_eq!(client.recv().await.unwrap(), b"hi");
            pairs.push((Rc::new(server), Rc::new(client)));
        }
        giong::time::sleep(Duration::from_millis(50)).await;
        let before = LIVE.load(Ordering::Relaxed);
        for (server, client) in &pairs {
            let client = client.clone();
            let reader = giong::spawn(async move {
                let mut got = 0;
                while got < 1 << 20 {
                    got += client.recv().await.unwrap().len();
                }
            });
            server.write_all(vec![7u8; 1 << 20]).await.0.unwrap();
            reader.await.unwrap();
        }
        giong::time::sleep(Duration::from_millis(50)).await;
        let kept = (LIVE.load(Ordering::Relaxed) - before) / PAIRS as isize;
        eprintln!("kept {kept} bytes per connection pair after one 1 MiB write");
        // 29 KB measured (rustls' receive buffers and its plaintext limit);
        // 95 KB with the outgoing buffer kept at its peak.
        assert!(kept < 48 * 1024, "{kept} bytes kept per connection pair");
        drop(pairs);
    });
}
