//! Small helpers for the words of error messages.

/// `T`'s name as written in Rust code: `Option<String>`, not
/// `core::option::Option<alloc::string::String>`.
pub(crate) fn rust_name<T: ?Sized>() -> String {
    short_path(std::any::type_name::<T>())
}

/// Drops the module path from every name in `full`.
fn short_path(full: &str) -> String {
    let mut out = String::with_capacity(full.len());
    // Where the name being written started in `out`.
    let mut name_start = 0;
    let mut chars = full.chars().peekable();
    while let Some(c) = chars.next() {
        if c == ':' && chars.peek() == Some(&':') {
            chars.next();
            out.truncate(name_start);
        } else {
            out.push(c);
            if !(c.is_alphanumeric() || c == '_') {
                name_start = out.len();
            }
        }
    }
    out
}

/// `word` with `a` or `an` before it, as it is said: `an integer`,
/// `an i32`, `a u64`, `a bigint`.
pub(crate) fn a(word: &str) -> String {
    let lower = word.trim_start_matches(['`', '&']).to_ascii_lowercase();
    let mut letters = lower.chars();
    let an = match letters.next() {
        Some('a' | 'e' | 'i' | 'o') => true,
        // "unsigned", but "u64", "uuid", "unique".
        Some('u') => {
            !(letters.next().is_some_and(|c| c.is_ascii_digit())
                || lower.starts_with("uu")
                || lower.starts_with("uni")
                || lower.starts_with("us"))
        }
        _ => false,
    };
    format!("{} {word}", if an { "an" } else { "a" })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_lose_their_module_paths() {
        assert_eq!(rust_name::<String>(), "String");
        assert_eq!(rust_name::<Option<String>>(), "Option<String>");
        assert_eq!(rust_name::<Vec<u8>>(), "Vec<u8>");
        assert_eq!(rust_name::<&[u8]>(), "&[u8]");
        assert_eq!(rust_name::<&str>(), "&str");
        assert_eq!(rust_name::<std::time::SystemTime>(), "SystemTime");
        assert_eq!(
            rust_name::<std::collections::HashMap<String, Vec<i64>>>(),
            "HashMap<String, Vec<i64>>"
        );
    }

    #[test]
    fn articles_follow_the_sound() {
        assert_eq!(a("integer"), "an integer");
        assert_eq!(a("i32"), "an i32");
        assert_eq!(a("int8"), "an int8");
        assert_eq!(a("u64"), "a u64");
        assert_eq!(a("uuid"), "a uuid");
        assert_eq!(a("unsigned integer"), "an unsigned integer");
        assert_eq!(a("bigint"), "a bigint");
        assert_eq!(a("text"), "a text");
        assert_eq!(a("ENUM"), "an ENUM");
        assert_eq!(a("&str"), "a &str");
        assert_eq!(a("SystemTime"), "a SystemTime");
    }
}
