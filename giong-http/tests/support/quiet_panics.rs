//! A panic hook for tests that make handlers panic on purpose: included with
//! `#[path]` by giong-http's own tests and by other crates' tests, so that
//! there is one copy.
//!
//! A handler's panic runs the hook on the worker that then answers 500 (or
//! ends the call). With `RUST_BACKTRACE` set, symbolizing a backtrace there
//! took 1.5 s a panic under ThreadSanitizer, past a test client's read
//! timeout on a loaded machine (#261). Panics whose message says "(expected
//! by this test)" print as one line, without a backtrace; every other panic
//! goes to the default hook, so a real failure keeps its backtrace.

/// Installs the hook, once per test binary.
pub fn quiet_expected_panics() {
    static QUIET: std::sync::Once = std::sync::Once::new();
    QUIET.call_once(|| {
        let default = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            let payload = info.payload();
            let message = payload
                .downcast_ref::<&str>()
                .copied()
                .or_else(|| payload.downcast_ref::<String>().map(String::as_str));
            if message.is_some_and(|m| m.contains("(expected by this test)")) {
                eprintln!("{info}");
            } else {
                default(info);
            }
        }));
    });
}
