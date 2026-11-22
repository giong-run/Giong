//! Telling, from its text, whether a statement may change the session: the
//! state a connection keeps between statements (variables, the default
//! database, locks, temporary tables), which must not reach the next task
//! that uses a pooled connection.
//!
//! The server reports most changes itself (see `protocol::Ok`), but not all:
//! MySQL 8.0 and MariaDB 10.11 were checked, and neither reports `LOCK
//! TABLES`, `GET_LOCK()`, `HANDLER ... OPEN`, `SET TRANSACTION`, or a user
//! variable assigned in a `SELECT` (`@v := ...`, `SELECT ... INTO @v`).
//! These checks cover what the server leaves out; both err on the side of
//! "it may".
//!
//! They run before every statement a pool shares, so they allocate nothing
//! and look for each mark (`@`, `:`, `_`) with the standard library's
//! word-at-a-time byte search.

/// The first words of a statement (four at most), after blanks and
/// comments, as written: compare them with [`Words::is`].
pub(crate) struct Words<'a> {
    words: [&'a str; 4],
    count: usize,
}

impl<'a> Words<'a> {
    pub(crate) fn of(sql: &'a str) -> Words<'a> {
        let b = sql.as_bytes();
        let mut w = Words {
            words: [""; 4],
            count: 0,
        };
        let mut i = 0;
        while w.count < 4 && i < b.len() {
            match b[i] {
                c if c.is_ascii_whitespace() => i += 1,
                // `-- ` needs a blank or control after the dashes in MySQL.
                b'-' if b.get(i + 1) == Some(&b'-')
                    && b.get(i + 2)
                        .map_or(true, |c| c.is_ascii_whitespace() || c.is_ascii_control()) =>
                {
                    i = line_end(b, i);
                }
                b'#' => i = line_end(b, i),
                b'/' if b.get(i + 1) == Some(&b'*') => {
                    // MySQL's block comments do not nest.
                    i = match sql[i + 2..].find("*/") {
                        Some(end) => i + 2 + end + 2,
                        None => b.len(),
                    };
                }
                c if c.is_ascii_alphabetic() || c == b'_' => {
                    let start = i;
                    while word_byte(b, i) {
                        i += 1;
                    }
                    w.words[w.count] = &sql[start..i];
                    w.count += 1;
                }
                _ => break,
            }
        }
        w
    }

    /// Word `i` as written, or `""`.
    pub(crate) fn get(&self, i: usize) -> &'a str {
        if i < self.count {
            self.words[i]
        } else {
            ""
        }
    }

    /// Whether word `i` is one of `keywords` (upper case), in any case.
    pub(crate) fn is(&self, i: usize, keywords: &[&str]) -> bool {
        let w = self.get(i);
        keywords.iter().any(|k| w.eq_ignore_ascii_case(k))
    }

    fn any(&self, keyword: &str) -> bool {
        self.words[..self.count]
            .iter()
            .any(|w| w.eq_ignore_ascii_case(keyword))
    }
}

fn line_end(b: &[u8], mut i: usize) -> usize {
    while i < b.len() && b[i] != b'\n' {
        i += 1;
    }
    i
}

/// Whether `b[i]` is part of a word (an identifier or a keyword).
fn word_byte(b: &[u8], i: usize) -> bool {
    b.get(i)
        .is_some_and(|c| c.is_ascii_alphanumeric() || *c == b'_')
}

/// What in a statement's text may touch the session, whatever its first
/// words.
#[derive(Default)]
struct Marks {
    /// `@name`, not `@@name` (a system variable).
    user_variable: bool,
    /// `... INTO @name`.
    into_variable: bool,
    /// `:=`.
    assignment: bool,
    /// `GET_LOCK`, as a word.
    named_lock: bool,
}

impl Marks {
    fn of(sql: &str) -> Marks {
        let b = sql.as_bytes();
        let mut m = Marks::default();
        if b.contains(&b'@') {
            for (i, _) in sql.match_indices('@') {
                let system = b.get(i + 1) == Some(&b'@') || (i > 0 && b[i - 1] == b'@');
                if system {
                    continue;
                }
                m.user_variable = true;
                // INTO, blanks, then the variable.
                let before = b[..i].trim_ascii_end();
                if before.len() < i
                    && before.len() >= 4
                    && before[before.len() - 4..].eq_ignore_ascii_case(b"INTO")
                    && !word_byte(b, before.len().wrapping_sub(5))
                {
                    m.into_variable = true;
                }
            }
        }
        if b.contains(&b':') {
            m.assignment = sql.contains(":=");
        }
        if b.contains(&b'_') {
            m.named_lock = sql.match_indices('_').any(|(i, _)| {
                i >= 3
                    && b[i - 3..i].eq_ignore_ascii_case(b"GET")
                    && b.get(i + 1..i + 5)
                        .is_some_and(|w| w.eq_ignore_ascii_case(b"LOCK"))
                    && !word_byte(b, i.wrapping_sub(4))
                    && !word_byte(b, i + 5)
            });
        }
        m
    }
}

/// `CREATE TEMPORARY TABLE`, `DROP TEMPORARY TABLE`, MariaDB's `CREATE OR
/// REPLACE TEMPORARY TABLE`.
fn temporary_table(words: &Words) -> bool {
    words.is(0, &["CREATE", "DROP"]) && words.any("TEMPORARY")
}

/// The statements that leave nothing in the session, unless they mention
/// a user variable or take a named lock: most often run first.
const PLAIN: &[&str] = &[
    "SELECT", "INSERT", "UPDATE", "DELETE", "REPLACE", "WITH", "VALUES", "TABLE", "DO", "SHOW",
    "EXPLAIN", "DESCRIBE", "DESC", "CREATE", "ALTER", "DROP", "RENAME", "TRUNCATE", "ANALYZE",
    "OPTIMIZE",
];

/// Whether a statement may leave something in the session. Only plain
/// reads, writes and schema changes that mention no user variable and take
/// no named lock are known not to.
pub(crate) fn may_change(sql: &str) -> bool {
    let words = Words::of(sql);
    let plain = words.is(0, PLAIN) && !temporary_table(&words);
    if !plain {
        return true;
    }
    let marks = Marks::of(sql);
    marks.user_variable || marks.named_lock
}

/// Why a statement must not run on a connection other tasks share, if it
/// changes that connection's session: the next statement of another task
/// would run in it, and the statement after it in this task may run on
/// another connection. `None` for statements that may run there.
#[cfg(test)]
pub(crate) fn changes(sql: &str) -> Option<&'static str> {
    changes_in(&Words::of(sql), sql)
}

/// [`changes`], with the statement's first words already read.
pub(crate) fn changes_in(words: &Words, sql: &str) -> Option<&'static str> {
    let reason = if words.is(0, &["SET"]) {
        // SET GLOBAL and PERSIST change the server, SET PASSWORD and SET
        // DEFAULT ROLE an account: not this session.
        let not_session = &["GLOBAL", "PERSIST", "PERSIST_ONLY", "PASSWORD", "DEFAULT"];
        (!words.is(1, not_session)).then_some("SET changes the session")
    } else if words.is(0, &["USE"]) {
        Some("USE changes the session's default database")
    } else if words.is(0, &["PREPARE", "EXECUTE", "DEALLOCATE"]) {
        Some("SQL prepared statements belong to the session")
    } else if words.is(0, &["HANDLER"]) {
        Some("a HANDLER belongs to the session")
    } else if words.is(0, &["CALL"]) {
        Some(
            "CALL runs a stored procedure, which may change the session (variables, a \
             transaction, locks) where the server does not report it",
        )
    } else if temporary_table(words) {
        Some("a temporary table belongs to the session")
    } else {
        None
    };
    reason.or_else(|| {
        let marks = Marks::of(sql);
        if marks.named_lock {
            Some("a named lock (GET_LOCK) belongs to the session")
        } else if marks.assignment || marks.into_variable {
            Some("a user variable belongs to the session")
        } else {
            None
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_reads_and_writes_leave_the_session_alone() {
        for sql in [
            "SELECT id FROM t WHERE x = ?",
            "  /* c */ insert into t values (?)",
            "UPDATE t SET a = ? WHERE id = ?",
            "DELETE FROM t",
            "REPLACE INTO t VALUES (1)",
            "WITH c AS (SELECT 1) SELECT * FROM c",
            "SELECT @@session.time_zone",
            "SELECT 1 FOR UPDATE",
            "SELECT LAST_INSERT_ID()",
            "CREATE TABLE t (temporary_flag INT)",
            "DROP TABLE IF EXISTS t",
            "ALTER TABLE t ADD COLUMN c INT",
            "DO SLEEP(1)",
            "SHOW TABLES",
            "SELECT target_locks, get_lock_count FROM t",
        ] {
            assert!(!may_change(sql), "{sql}");
            assert_eq!(changes(sql), None, "{sql}");
        }
    }

    #[test]
    fn statements_that_change_the_session_are_seen() {
        for sql in [
            "SET SESSION sql_mode = ''",
            "set @x = 1",
            "SET @@session.time_zone = '+07:00'",
            "SET TRANSACTION ISOLATION LEVEL SERIALIZABLE",
            "SET NAMES latin1",
            "SET ROLE NONE",
            "USE mysql",
            "PREPARE s FROM 'SELECT 1'",
            "EXECUTE s",
            "DEALLOCATE PREPARE s",
            "HANDLER t OPEN",
            "CREATE TEMPORARY TABLE x (a INT)",
            "CREATE OR REPLACE TEMPORARY TABLE x (a INT)",
            "drop temporary table x",
            "SELECT GET_LOCK('a', 0)",
            "select get_lock ('a', 0)",
            "SELECT 1, IS_FREE_LOCK('a'), GET_LOCK('a', 0)",
            "SELECT @n := @n + 1 FROM t",
            "SELECT a INTO @v FROM t",
            "SELECT a FROM t INTO\n  @v",
            "CALL p()",
            "call db.p(?)",
        ] {
            assert!(may_change(sql), "{sql}");
            assert!(changes(sql).is_some(), "{sql}");
        }
        // Not this session's: allowed on shared connections.
        for sql in [
            "SET GLOBAL max_connections = 10",
            "SET PERSIST max_connections = 10",
            "SET PASSWORD = 'x'",
        ] {
            assert_eq!(changes(sql), None, "{sql}");
        }
        // Anything but a plain read or write may change the session when
        // a connection is given back.
        for sql in [
            "LOCK TABLES t WRITE",
            "FLUSH TABLES WITH READ LOCK",
            "DO GET_LOCK('a', 0)",
            "SELECT @v",
            "CREATE PROCEDURE p() SET @x = 1",
        ] {
            assert!(may_change(sql), "{sql}");
        }
        assert!(!Marks::of("SELECT a INTO OUTFILE '/x'").into_variable);
        assert!(!Marks::of("INSERT INTO t VALUES ('@')").into_variable);
        assert!(!Marks::of("SELECT a FROM t WHERE x = @@sql_mode").user_variable);
    }
}
