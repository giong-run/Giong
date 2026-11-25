//! What a value looks like, for masks.

/// `2026-11-22T08:40:01Z`, `2026-11-22T08:40:01.123456+07:00`,
/// `2026-11-22 08:40:01`: an RFC 3339 date and time (space allowed).
pub fn is_time(s: &str) -> bool {
    let b = s.as_bytes();
    if b.len() < 19 || b.len() > 40 {
        return false;
    }
    let digits = |r: std::ops::Range<usize>| b[r].iter().all(u8::is_ascii_digit);
    if !(digits(0..4)
        && b[4] == b'-'
        && digits(5..7)
        && b[7] == b'-'
        && digits(8..10)
        && (b[10] == b'T' || b[10] == b' ' || b[10] == b't')
        && digits(11..13)
        && b[13] == b':'
        && digits(14..16)
        && b[16] == b':'
        && digits(17..19))
    {
        return false;
    }
    let mut rest = &b[19..];
    if let Some(r) = rest.strip_prefix(b".") {
        let n = r.iter().take_while(|c| c.is_ascii_digit()).count();
        if n == 0 {
            return false;
        }
        rest = &r[n..];
    }
    match rest {
        [] | [b'Z'] | [b'z'] => true,
        [b'+' | b'-', h1, h2, b':', m1, m2] => [h1, h2, m1, m2].iter().all(|c| c.is_ascii_digit()),
        _ => false,
    }
}

/// `550e8400-e29b-41d4-a716-446655440000`, any case.
pub fn is_uuid(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() == 36
        && b.iter().enumerate().all(|(i, c)| match i {
            8 | 13 | 18 | 23 => *c == b'-',
            _ => c.is_ascii_hexdigit(),
        })
}

/// A long hex string with letters and digits, such as a request id or
/// a hash: 16 hex characters or more.
pub fn is_hex_id(s: &str) -> bool {
    s.len() >= 16
        && s.bytes().all(|c| c.is_ascii_hexdigit())
        && s.bytes().any(|c| c.is_ascii_digit())
        && s.bytes().any(|c| c.is_ascii_alphabetic())
}

/// A key that names an identifier: `id`, `user_id`, `userId`, `uuid`.
pub fn is_id_key(key: &str) -> bool {
    let k = key.to_ascii_lowercase();
    k == "id"
        || k == "uuid"
        || k == "guid"
        || k.ends_with("_id")
        || k.ends_with("-id")
        || k.ends_with("_uuid")
        || (key.ends_with("Id") && key.len() > 2)
}

/// A key whose value is a moment: `created_at`, `updatedAt`, `expires`.
pub fn is_time_key(key: &str) -> bool {
    let k = key.to_ascii_lowercase();
    k.ends_with("_at")
        || (key.ends_with("At") && key.len() > 2)
        || k.ends_with("_time")
        || k.ends_with("timestamp")
        || matches!(
            k.as_str(),
            "date" | "time" | "ts" | "now" | "expires" | "created" | "updated" | "modified"
        )
}

/// A key whose value is a secret or changes every time: tokens, nonces.
pub fn is_token_key(key: &str) -> bool {
    let k = key.to_ascii_lowercase();
    k.contains("token")
        || k.contains("nonce")
        || k.contains("secret")
        || k.contains("password")
        || k.contains("csrf")
        || k == "etag"
        || k == "session"
        || k == "signature"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn times() {
        for ok in [
            "2026-11-22T08:40:01Z",
            "2026-11-22T08:40:01.123456Z",
            "2026-11-22T08:40:01+07:00",
            "2026-11-22 08:40:01",
            "2026-11-22t08:40:01.5-00:30",
        ] {
            assert!(is_time(ok), "{ok}");
        }
        for bad in [
            "2026-11-22",
            "2026-11-22T08:40:01.Z",
            "2026-11-22T08:40:01+0700",
            "2026-11-22T08:40:01Zjunk",
            "abcd-11-22T08:40:01Z",
            "",
        ] {
            assert!(!is_time(bad), "{bad}");
        }
    }

    #[test]
    fn uuids_hex_and_keys() {
        assert!(is_uuid("550e8400-e29b-41d4-A716-446655440000"));
        assert!(!is_uuid("550e8400e29b41d4a716446655440000"));
        assert!(!is_uuid("550e8400-e29b-41d4-a716-44665544000g"));
        assert!(is_hex_id("5f3a9c2e8b41d706000100000000002a"));
        assert!(!is_hex_id("0000000000000000"));
        assert!(!is_hex_id("deadbeefdeadbeef"));
        assert!(!is_hex_id("5f3a9c2e"));
        for k in ["id", "ID", "user_id", "userId", "uuid", "order-id"] {
            assert!(is_id_key(k), "{k}");
        }
        for k in ["idle", "valid", "paid", "width", "I"] {
            assert!(!is_id_key(k), "{k}");
        }
        for k in ["created_at", "updatedAt", "ts", "expires", "start_time"] {
            assert!(is_time_key(k), "{k}");
        }
        assert!(!is_time_key("status"));
        for k in ["access_token", "csrfToken", "etag", "nonce"] {
            assert!(is_token_key(k), "{k}");
        }
        assert!(!is_token_key("name"));
    }
}
