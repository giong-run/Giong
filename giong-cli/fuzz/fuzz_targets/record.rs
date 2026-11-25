//! `giong record`: any log line parses without a panic (most lines of a log
//! are not exchanges), and matching a case template against any answer ends
//! quickly without a panic. A template with no placeholder matches itself,
//! and `{{any}}` matches everything.
#![no_main]

use giong_cli_fuzz::record::exchange::parse_line;
use giong_cli_fuzz::record::matcher::{match_text, parts, Vars};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let Ok(text) = std::str::from_utf8(data) else {
        return;
    };
    for (i, line) in text.lines().enumerate() {
        let _ = parse_line(line, i + 1);
    }
    // First line: a template; the rest: the answer.
    let (template, actual) = text.split_once('\n').unwrap_or((text, ""));
    let _ = parts(template);
    let start = std::time::Instant::now();
    let _ = match_text(template, actual, &mut Vars::new());
    assert!(start.elapsed() < std::time::Duration::from_secs(2), "slow match");
    assert!(match_text("{{any}}", actual, &mut Vars::new()).is_ok());
    if !actual.contains("{{") && !actual.contains("}}") {
        assert!(
            match_text(actual, actual, &mut Vars::new()).is_ok(),
            "a plain text does not match itself: {actual:?}"
        );
    }
});
