//! The dashboard reads requests from any program on this machine (and any
//! web page the browser has open): no panic on any bytes, and a request that
//! `admit` lets in names one of the dashboard's own loopback hosts, and an
//! API request carries this start's token.
#![no_main]

use giong_cli_fuzz::dashboard_http::{admit, read_request};
use libfuzzer_sys::fuzz_target;

const TOKEN: &str = "fuzz-token";

fuzz_target!(|data: &[u8]| {
    let mut r = data;
    let Ok(req) = read_request(&mut r) else {
        return;
    };
    if admit(&req, 7700, TOKEN).is_ok() {
        let hosts: Vec<&str> = req
            .headers
            .iter()
            .filter(|(n, _)| n == "host")
            .map(|(_, v)| v.as_str())
            .collect();
        assert!(!hosts.is_empty(), "admitted with no Host");
        for h in hosts {
            assert!(
                ["127.0.0.1:7700", "localhost:7700", "[::1]:7700"]
                    .iter()
                    .any(|a| a.eq_ignore_ascii_case(h)),
                "admitted with Host {h:?}"
            );
        }
        if req.path.starts_with("/api/") {
            assert_eq!(
                req.header("x-giong-dashboard"),
                Some(TOKEN),
                "an API request admitted without the token"
            );
        }
    }
});
