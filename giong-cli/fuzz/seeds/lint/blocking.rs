use std::cell::RefCell;
use std::time::Duration;

async fn tick() {}

async fn blocking() -> String {
    std::thread::sleep(Duration::from_millis(1));
    std::fs::read_to_string("x").unwrap_or_default()
}

fn fine_sync() {
    // std::thread::sleep(Duration::from_millis(1)) in a comment
    let _s = "std::thread::sleep in a string";
    std::thread::sleep(Duration::from_millis(1));
}

async fn borrow(c: &RefCell<u32>) {
    let g = c.borrow_mut();
    tick().await;
    drop(g);
}

async fn bind() {
    let _ = std::net::TcpListener::bind("localhost:8080");
}

fn main() {
    let v = vec![1, 2, 3];
    let _n = v.iter().count();
    fine_sync();
    let _ = (blocking(), borrow(&RefCell::new(0)), bind());
}
