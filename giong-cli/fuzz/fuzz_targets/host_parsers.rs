//! What doctor reads from rustc, the kernel and /proc: any text gives an
//! answer or `None`, never a panic, and a version printed back reads back.
#![no_main]

use giong_cli_fuzz::host::{parse_limit, parse_seccomp, parse_version, Version};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let text = String::from_utf8_lossy(data);
    if let Some(v) = parse_version(&text) {
        assert_eq!(parse_version(&v.to_string()), Some(v));
        assert_eq!(parse_version(&format!("rustc {v}-nightly (abc 2026-01-01)")), Some(v));
        let _ = v < Version(1, 80, 0);
    }
    for name in ["Max open files", "Max locked memory", ""] {
        if let Some(l) = parse_limit(&text, name) {
            if let (Some(s), Some(h)) = (l.soft, l.hard) {
                let _ = s <= h;
            }
        }
    }
    let _ = parse_seccomp(&text);
});
