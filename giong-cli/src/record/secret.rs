//! Secrets in recorded exchanges: field names that say secret, and values
//! that look like one (a card number, a JWT).
//!
//! `giong record save` reads secret-named request fields from the
//! environment when replayed instead of writing them into the case file,
//! and refuses to write a value that still looks like a secret: case files
//! are tests, meant to be committed.

use serde_json::Value;

use super::matcher::{self, Part};

/// Words that make a name secret wherever they stand in it.
const WORDS: [&str; 12] = [
    "password",
    "passwd",
    "pwd",
    "passphrase",
    "secret",
    "token",
    "apikey",
    "cvv",
    "cvc",
    "ssn",
    "authorization",
    "card",
];

/// Words that make a following `key` secret (`api_key`, `privateKey`).
const KEY_OWNERS: [&str; 5] = ["api", "private", "secret", "access", "signing"];

/// The lower-case words of a name: split at anything not a letter or digit
/// and where a lower-case letter or digit meets an upper-case one
/// (`newPassword` is `new`, `password`).
fn words(name: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut prev_lower = false;
    for c in name.chars() {
        if !c.is_ascii_alphanumeric() {
            if !cur.is_empty() {
                out.push(std::mem::take(&mut cur));
            }
            prev_lower = false;
            continue;
        }
        if c.is_ascii_uppercase() && prev_lower && !cur.is_empty() {
            out.push(std::mem::take(&mut cur));
        }
        prev_lower = c.is_ascii_lowercase() || c.is_ascii_digit();
        cur.push(c.to_ascii_lowercase());
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

/// Whether a field, query parameter or header name says its value is a
/// secret: `password`, `newPassword`, `client_secret`, `access_token`,
/// `x-api-key`, `private_key`, `card_number`, `cvv`. Not `key`, `sort_key`,
/// `cardinality` or `monkey`.
pub fn is_secret_name(name: &str) -> bool {
    let w = words(name);
    if w.iter().any(|x| WORDS.contains(&x.as_str())) {
        return true;
    }
    // A bare `key` is as often a resource's name (an object key, a sort
    // key the service made and a later request uses) as a secret.
    match w.as_slice() {
        [.., owner, last] => last == "key" && KEY_OWNERS.contains(&owner.as_str()),
        _ => false,
    }
}

/// The Luhn check card numbers pass.
fn luhn(digits: &[u8]) -> bool {
    let mut sum = 0u32;
    for (i, d) in digits.iter().rev().enumerate() {
        let mut v = u32::from(d - b'0');
        if i % 2 == 1 {
            v *= 2;
            if v > 9 {
                v -= 9;
            }
        }
        sum += v;
    }
    sum % 10 == 0
}

/// Whether `digits` (13 to 19 of them) read as a payment card number: a
/// card network's first digit (2 to 6) and the Luhn check. Times in
/// milliseconds start with 1, so they are not taken for one.
fn is_card(digits: &[u8]) -> bool {
    (13..=19).contains(&digits.len()) && (b'2'..=b'6').contains(&digits[0]) && luhn(digits)
}

/// The card numbers in `s`: runs of digits, single spaces or dashes
/// allowed between groups (`4242 4242 4242 4242`), not inside a longer
/// word or number.
fn has_card(s: &str) -> bool {
    let b = s.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if !b[i].is_ascii_digit() || (i > 0 && b[i - 1].is_ascii_alphanumeric()) {
            i += 1;
            continue;
        }
        let mut digits = Vec::new();
        let mut j = i;
        while j < b.len() {
            if b[j].is_ascii_digit() {
                digits.push(b[j]);
                j += 1;
            } else if (b[j] == b' ' || b[j] == b'-')
                && j + 1 < b.len()
                && b[j + 1].is_ascii_digit()
                && j > i
                && b[j - 1].is_ascii_digit()
            {
                j += 1;
            } else {
                break;
            }
        }
        let ends_clean = j == b.len() || !b[j].is_ascii_alphanumeric();
        if ends_clean && is_card(&digits) {
            return true;
        }
        i = j.max(i + 1);
    }
    false
}

/// Whether `s` holds a JWT: three base64url parts joined by dots, the first
/// starting `eyJ` (`{"` encoded).
fn has_jwt(s: &str) -> bool {
    let b = s.as_bytes();
    let part = |from: usize| {
        b[from..]
            .iter()
            .take_while(|c| c.is_ascii_alphanumeric() || **c == b'-' || **c == b'_')
            .count()
    };
    let mut from = 0;
    while let Some(off) = s[from..].find("eyJ") {
        let i = from + off;
        from = i + 3;
        if i > 0 && (b[i - 1].is_ascii_alphanumeric() || b[i - 1] == b'_' || b[i - 1] == b'-') {
            continue;
        }
        let h = part(i);
        if h < 10 || i + h >= b.len() || b[i + h] != b'.' {
            continue;
        }
        let p = part(i + h + 1);
        if p < 10 || i + h + 1 + p >= b.len() || b[i + h + 1 + p] != b'.' {
            continue;
        }
        return true;
    }
    false
}

/// What a value looks like, if a secret: "a card number" or "a JWT".
/// Placeholders (`{{env:...}}`, `{{string}}`) are not looked into.
pub fn looks_secret(s: &str) -> Option<&'static str> {
    for p in matcher::parts(s) {
        if let Part::Lit(l) = p {
            if has_jwt(l) {
                return Some("a JWT");
            }
            if has_card(l) {
                return Some("a card number");
            }
        }
    }
    None
}

/// The secrets left in a saved case (its JSON form), as (where, what):
/// `request.body.card.number: a card number`. A value under a secret name
/// counts unless it is a placeholder or empty.
pub fn scan(case: &Value) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let req = &case["request"];
    if let Some(p) = req["path"].as_str() {
        if let Some(what) = looks_secret(p) {
            out.push(("request.path".into(), what.into()));
        }
    }
    if let Some(q) = req["query"].as_str() {
        for pair in q.split('&') {
            let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
            let k = percent_decode(k);
            let v = percent_decode(v);
            if let Some(what) = judge(Some(&k), &v) {
                out.push((format!("request.query.{k}"), what));
            }
        }
    }
    for (n, v) in headers(&req["headers"]) {
        if let Some(what) = judge(Some(n), v) {
            out.push((format!("request.headers.{n}"), what));
        }
    }
    body(req, "request", &mut out);
    let exp = &case["expect"];
    for (n, v) in headers(&exp["headers"]) {
        if let Some(what) = looks_secret(v) {
            out.push((format!("expect.headers.{n}"), what.into()));
        }
    }
    body(exp, "expect", &mut out);
    out
}

/// A case's headers: a list of `[name, value]` (requests) or an object
/// (expectations).
fn headers(v: &Value) -> Vec<(&str, &str)> {
    match v {
        Value::Array(a) => a
            .iter()
            .filter_map(|p| Some((p.get(0)?.as_str()?, p.get(1)?.as_str()?)))
            .collect(),
        Value::Object(o) => o
            .iter()
            .filter_map(|(n, v)| Some((n.as_str(), v.as_str()?)))
            .collect(),
        _ => Vec::new(),
    }
}

/// A request's or expectation's body: `body` (text) or `body_json`.
fn body(part: &Value, at: &str, out: &mut Vec<(String, String)>) {
    if let Some(t) = part["body"].as_str() {
        if let Some(what) = looks_secret(t) {
            out.push((format!("{at}.body"), what.into()));
        }
    }
    walk(&part["body_json"], &format!("{at}.body"), None, out);
}

/// Why `value` (under `name`) is a secret, if it is one.
fn judge(name: Option<&str>, value: &str) -> Option<String> {
    let is_placeholder = matcher::parts(value)
        .iter()
        .all(|p| matches!(p, Part::Ph(_)));
    if value.is_empty() || is_placeholder || value == "[redacted]" {
        return None;
    }
    if let Some(what) = looks_secret(value) {
        return Some(what.into());
    }
    match name {
        Some(n) if is_secret_name(n) => Some(format!("a value under the secret name {n:?}")),
        _ => None,
    }
}

fn walk(v: &Value, at: &str, key: Option<&str>, out: &mut Vec<(String, String)>) {
    match v {
        Value::Object(o) => {
            for (k, x) in o {
                walk(x, &format!("{at}.{k}"), Some(k), out);
            }
        }
        Value::Array(a) => {
            for (i, x) in a.iter().enumerate() {
                walk(x, &format!("{at}[{i}]"), key, out);
            }
        }
        Value::String(s) => {
            if let Some(what) = judge(key, s) {
                out.push((at.to_string(), what));
            }
        }
        Value::Number(n) => {
            if let Some(what) = judge(key, &n.to_string()) {
                out.push((at.to_string(), what));
            }
        }
        Value::Null | Value::Bool(_) => {}
    }
}

/// `%XX` and `+` decoded, for reading query names and values; bytes that do
/// not make UTF-8 are replaced.
pub fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'+' => out.push(b' '),
            b'%' if i + 2 < b.len() => {
                let hex = std::str::from_utf8(&b[i + 1..i + 3])
                    .ok()
                    .filter(|h| h.bytes().all(|c| c.is_ascii_hexdigit()))
                    .and_then(|h| u8::from_str_radix(h, 16).ok());
                match hex {
                    Some(x) => {
                        out.push(x);
                        i += 3;
                        continue;
                    }
                    None => out.push(b'%'),
                }
            }
            c => out.push(c),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn secret_names() {
        for n in [
            "password",
            "Password",
            "newPassword",
            "new_password",
            "passwd",
            "pwd",
            "client_secret",
            "clientSecret",
            "access_token",
            "refresh_token",
            "token",
            "api_key",
            "apiKey",
            "x-api-key",
            "apikey",
            "API_KEY",
            "private_key",
            "secretKey",
            "card",
            "card_number",
            "cardNumber",
            "cvv",
            "cvc",
            "ssn",
            "authorization",
            "X-Stripe-Secret",
        ] {
            assert!(is_secret_name(n), "{n}");
        }
        for n in [
            "key",
            "sort_key",
            "primaryKey",
            "monkey",
            "keyboard",
            "cardinality",
            "discard",
            "username",
            "email",
            "id",
            "page",
            "tokenizer_version_x",
        ] {
            // `tokenizer` is one word, not `token`.
            assert!(!is_secret_name(n), "{n}");
        }
    }

    #[test]
    fn card_numbers_by_luhn() {
        assert_eq!(looks_secret("4242424242424242"), Some("a card number"));
        assert_eq!(looks_secret("4242 4242 4242 4242"), Some("a card number"));
        assert_eq!(looks_secret("4242-4242-4242-4242"), Some("a card number"));
        assert_eq!(
            looks_secret("card 5555555555554444 ok"),
            Some("a card number")
        );
        assert_eq!(looks_secret("378282246310005"), Some("a card number"));
        // Fails Luhn.
        assert_eq!(looks_secret("4242424242424241"), None);
        // A time in milliseconds, and a too-short or too-long number.
        assert_eq!(looks_secret("1732345678901"), None);
        assert_eq!(looks_secret("424242424242"), None);
        assert_eq!(looks_secret("42424242424242424242"), None);
        // Inside a word or a longer number.
        assert_eq!(looks_secret("x4242424242424242"), None);
        assert_eq!(looks_secret("4242424242424242x"), None);
    }

    #[test]
    fn jwts() {
        let jwt = "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.dozjgNryP4J3jVmNHl0w5N_XgL0n3I9PlFUP0THsR8U";
        assert_eq!(looks_secret(jwt), Some("a JWT"));
        assert_eq!(looks_secret(&format!("Bearer {jwt}")), Some("a JWT"));
        assert_eq!(looks_secret(&format!("{{\"t\":\"{jwt}\"}}")), Some("a JWT"));
        assert_eq!(looks_secret("eyJhbGciOiJIUzI1NiJ9"), None);
        assert_eq!(looks_secret("see eyJ.short.parts"), None);
    }

    #[test]
    fn placeholders_are_not_secrets() {
        assert_eq!(looks_secret("{{env:GIONG_RECORD_PASSWORD}}"), None);
        let case = json!({
            "request": {
                "path": "/login",
                "query": "api_key={{env:GIONG_RECORD_API_KEY}}&page=2",
                "headers": [["authorization", "{{env:GIONG_RECORD_AUTHORIZATION}}"]],
                "body_json": {"user": "ana", "password": "{{env:GIONG_RECORD_PASSWORD}}", "token": ""}
            },
            "expect": {"status": 200, "body_json": {"token": "{{string}}"}}
        });
        assert!(scan(&case).is_empty(), "{:?}", scan(&case));
    }

    #[test]
    fn what_is_left_is_found_and_named() {
        let case = json!({
            "request": {
                "path": "/pay",
                "query": "api%5Fkey=AK-SECRET-123",
                "headers": [["x-session-token", "abc"]],
                "body_json": {"card": {"number": "4242424242424242"}, "items": [{"cvv": 123}]}
            },
            "expect": {
                "status": 200,
                "headers": {"location": "/done?t=eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.sig"},
                "body": "paid with 5555555555554444"
            }
        });
        let found: Vec<String> = scan(&case).into_iter().map(|(at, _)| at).collect();
        assert_eq!(
            found,
            [
                "request.query.api_key",
                "request.headers.x-session-token",
                "request.body.card.number",
                "request.body.items[0].cvv",
                "expect.headers.location",
                "expect.body",
            ]
        );
    }

    #[test]
    fn percent_decoding() {
        assert_eq!(percent_decode("api%5Fkey"), "api_key");
        assert_eq!(percent_decode("a+b%20c"), "a b c");
        assert_eq!(percent_decode("100%"), "100%");
        assert_eq!(percent_decode("%zz%4"), "%zz%4");
        assert_eq!(percent_decode("%+5"), "% 5");
    }
}
