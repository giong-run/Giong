//! The `Date` header value, formatted at most once per second per thread.

use std::cell::Cell;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

thread_local! {
    /// The text, and the time on the worker's clock until which it holds:
    /// the start of the next second.
    static CACHE: Cell<(Option<Instant>, [u8; 29])> = const { Cell::new((None, [0; 29])) };
}

/// The current time as an IMF-fixdate, e.g. `Sun, 06 Nov 1994 08:49:37 GMT`.
///
/// The worker reads its clock once per turn of its event loop
/// (`giong::time::turn_clock`), so a response costs no clock read of its own
/// while its second lasts. As that clock is never ahead, the text changes at
/// the second or a little before it, never early by a whole second; within
/// a turn that crosses the second, it stays the earlier one, as nginx's
/// cached time does.
pub(crate) fn now() -> [u8; 29] {
    let clock = giong::time::turn_clock();
    CACHE.with(|cache| {
        let (until, text) = cache.get();
        if until.is_some_and(|until| clock < until) {
            return text;
        }
        let since = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default();
        let text = format(since.as_secs());
        let left = Duration::from_secs(1) - Duration::from_nanos(u64::from(since.subsec_nanos()));
        cache.set((Some(clock + left), text));
        text
    })
}

pub(crate) fn format(secs: u64) -> [u8; 29] {
    const DAYS: [&[u8; 3]; 7] = [b"Thu", b"Fri", b"Sat", b"Sun", b"Mon", b"Tue", b"Wed"];
    const MONTHS: [&[u8; 3]; 12] = [
        b"Jan", b"Feb", b"Mar", b"Apr", b"May", b"Jun", b"Jul", b"Aug", b"Sep", b"Oct", b"Nov",
        b"Dec",
    ];
    let days = secs / 86_400;
    let rem = secs % 86_400;
    // Civil date from a day count (Howard Hinnant's algorithm).
    let z = days as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u64;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as usize;
    let year = (yoe + era * 400 + i64::from(month <= 2)).clamp(0, 9999) as u64;

    let mut out = *b"Thu, 01 Jan 1970 00:00:00 GMT";
    let two = |out: &mut [u8; 29], at: usize, v: u64| {
        out[at] = b'0' + (v / 10 % 10) as u8;
        out[at + 1] = b'0' + (v % 10) as u8;
    };
    out[..3].copy_from_slice(DAYS[(days % 7) as usize]);
    two(&mut out, 5, day);
    out[8..11].copy_from_slice(MONTHS[month - 1]);
    two(&mut out, 12, year / 100);
    two(&mut out, 14, year % 100);
    two(&mut out, 17, rem / 3600);
    two(&mut out, 20, rem / 60 % 60);
    two(&mut out, 23, rem % 60);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_known_instants() {
        let cases: &[(u64, &str)] = &[
            (0, "Thu, 01 Jan 1970 00:00:00 GMT"),
            (784_111_777, "Sun, 06 Nov 1994 08:49:37 GMT"),
            (951_782_400, "Tue, 29 Feb 2000 00:00:00 GMT"),
            (1_709_251_199, "Thu, 29 Feb 2024 23:59:59 GMT"),
            (1_709_251_200, "Fri, 01 Mar 2024 00:00:00 GMT"),
            (1_794_412_800, "Wed, 11 Nov 2026 16:00:00 GMT"),
            (4_102_444_799, "Thu, 31 Dec 2099 23:59:59 GMT"),
        ];
        for (secs, expected) in cases {
            assert_eq!(std::str::from_utf8(&format(*secs)).unwrap(), *expected);
        }
        assert_eq!(now().len(), 29);
        assert!(now().ends_with(b" GMT"));
    }

    fn wall_clock_secs() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs()
    }

    /// The text read from the worker's clock is the wall clock's second,
    /// and moves on once that second is over.
    #[test]
    fn follows_the_wall_clock_on_a_worker() {
        giong::block_on(async {
            for _ in 0..2 {
                let before = wall_clock_secs();
                let text = now();
                let after = wall_clock_secs();
                assert!(
                    (before..=after).any(|secs| format(secs) == text),
                    "{:?} is not between {before} and {after}",
                    std::str::from_utf8(&text)
                );
                giong::time::sleep(Duration::from_millis(1100)).await;
            }
        });
    }
}
