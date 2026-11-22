//! Recorded requests: the `exchange` lines giong-web's `Record` middleware
//! writes, turned into replayable cases (`giong record save`) and replayed
//! against the running service (`giong record replay`).
//!
//! - [`exchange`]: reading `exchange` lines from a log.
//! - [`case`]: making cases from exchanges, with volatile values masked and
//!   ids the service made captured and reused by later requests.
//! - [`matcher`]: checking an answer against a case's expectation.
//! - [`replay`]: sending a case file's requests and checking the answers.

pub mod case;
pub mod exchange;
pub mod matcher;
pub mod replay;

/// What a value looks like, for masks.
pub mod shape {
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
            [b'+' | b'-', h1, h2, b':', m1, m2] => {
                [h1, h2, m1, m2].iter().all(|c| c.is_ascii_digit())
            }
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
}

/// Standard base64 (RFC 4648), as `Record` writes it.
pub mod base64 {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

    pub fn encode(input: &[u8]) -> String {
        let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
        for chunk in input.chunks(3) {
            let b = [
                chunk[0],
                chunk.get(1).copied().unwrap_or(0),
                chunk.get(2).copied().unwrap_or(0),
            ];
            let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
            out.push(TABLE[(n >> 18) as usize & 63] as char);
            out.push(TABLE[(n >> 12) as usize & 63] as char);
            out.push(if chunk.len() > 1 {
                TABLE[(n >> 6) as usize & 63] as char
            } else {
                '='
            });
            out.push(if chunk.len() > 2 {
                TABLE[n as usize & 63] as char
            } else {
                '='
            });
        }
        out
    }

    /// Decodes padded or unpadded base64; `None` on any other character.
    pub fn decode(input: &str) -> Option<Vec<u8>> {
        let val = |c: u8| -> Option<u32> {
            Some(u32::from(match c {
                b'A'..=b'Z' => c - b'A',
                b'a'..=b'z' => c - b'a' + 26,
                b'0'..=b'9' => c - b'0' + 52,
                b'+' => 62,
                b'/' => 63,
                _ => return None,
            }))
        };
        let s = input.trim_end_matches('=').as_bytes();
        if input.len() - s.len() > 2 || s.len() % 4 == 1 {
            return None;
        }
        let mut out = Vec::with_capacity(s.len() * 3 / 4);
        for chunk in s.chunks(4) {
            let mut n = 0u32;
            for &c in chunk {
                n = (n << 6) | val(c)?;
            }
            n <<= 6 * (4 - chunk.len() as u32);
            let bytes = [(n >> 16) as u8, (n >> 8) as u8, n as u8];
            out.extend_from_slice(&bytes[..chunk.len() - 1]);
        }
        Some(out)
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn round_trips_and_rfc_examples() {
            for (plain, coded) in [
                ("", ""),
                ("f", "Zg=="),
                ("fo", "Zm8="),
                ("foo", "Zm9v"),
                ("foob", "Zm9vYg=="),
                ("fooba", "Zm9vYmE="),
                ("foobar", "Zm9vYmFy"),
            ] {
                assert_eq!(encode(plain.as_bytes()), coded);
                assert_eq!(decode(coded).unwrap(), plain.as_bytes());
                assert_eq!(
                    decode(coded.trim_end_matches('=')).unwrap(),
                    plain.as_bytes()
                );
            }
            let all: Vec<u8> = (0..=255).collect();
            assert_eq!(decode(&encode(&all)).unwrap(), all);
            assert!(decode("Zg=!").is_none());
            assert!(decode("Z").is_none());
            assert!(decode("Zg===").is_none());
        }
    }
}
