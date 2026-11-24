//! The codes the `giong` command reports: `GIONG-E5000` to `GIONG-E5099`.
//!
//! Ranges (one owner each, so work in parallel never collides):
//!
//! | Codes | Owner (thread) | Commands |
//! |---|---|---|
//! | E5000-E5029 | A | the command itself, `doctor`, `check`, `explain`, `clean`, `mcp`, `dev` |
//! | E5030-E5039, E5080-E5089 | B | `new`, `ai`, `fix` |
//! | E5040-E5049 | C | `db`, config |
//! | E5050-E5054 | D | routes, `openapi`, dashboard |
//! | E5055-E5059 | C | `top` |
//! | E5060-E5064 | C | `bench` |
//! | E5065-E5069 | D | recording |
//! | E5070-E5079 | C | `image` |
//! | E5090-E5099 | | free |
//!
//! Findings may also carry the runtime's own codes (`GIONG-E0001` when io_uring
//! is blocked, for example): [`Code::runtime`] makes one. Every code here has a
//! page `docs/errors/E50xx.md` and a row in `docs/errors/README.md`, checked by
//! the tests below (the runtime's `tests/error_docs.rs` leaves E50xx to them).

use std::fmt;

/// A `GIONG-Exxxx` code with its one-line summary.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct Code {
    number: u16,
    name: &'static str,
    summary: &'static str,
}

macro_rules! codes {
    ($($konst:ident = $number:literal, $name:literal, $summary:literal;)*) => {
        impl Code {
            $(
                #[doc = $summary]
                pub const $konst: Code = Code { number: $number, name: $name, summary: $summary };
            )*
        }
        /// Every code of the command, in order.
        pub const ALL: &[Code] = &[$(Code::$konst),*];
    };
}

codes! {
    USAGE = 5000, "Usage", "the command line is not valid";
    NOT_A_PROJECT = 5001, "NotAProject", "no Cargo project here";
    CARGO_FAILED = 5002, "CargoFailed", "cargo could not be run";
    RUST_TOO_OLD = 5003, "RustTooOld", "the Rust compiler is too old";
    TOKIO_CRATE = 5004, "TokioCrate", "a dependency needs the Tokio runtime";
    UNKNOWN_CODE = 5005, "UnknownCode", "no such error code";
    BLOCKING_CALL = 5006, "BlockingCall", "a blocking call inside a worker";
    HOST_NAME_BIND = 5007, "HostNameBind", "binding a host name";
    BORROW_ACROSS_AWAIT = 5008, "BorrowAcrossAwait", "a RefCell borrow held across an await";
    BUILD_FAILED = 5009, "BuildFailed", "the code does not compile";
    LINT = 5010, "Lint", "clippy has a finding";
    DISK_USED = 5011, "DiskUsed", "build directories use a lot of disk";
    SLOW_DEBUG_INFO = 5012, "SlowDebugInfo", "the dev profile writes full debug information";
    OPEN_FILES_LIMIT = 5013, "OpenFilesLimit", "the open-files limit is low";
    PORT_IN_USE = 5014, "PortInUse", "the port is in use";
    DOCKER_SECCOMP = 5015, "DockerSeccomp", "Docker blocks io_uring";
    ENV_MISSING = 5016, "EnvMissing", "a variable from .env.example is not set";
    SERVICE_EXITED = 5017, "ServiceExited", "the service stopped";
    SERVICE_NOT_READY = 5018, "ServiceNotReady", "the service did not start listening";
    MCP_UNREACHABLE = 5019, "McpUnreachable", "the service's giong-mcp does not answer";
    WATCH_LIMIT = 5020, "WatchLimit", "too many files to watch";
    NO_LINKER = 5021, "NoLinker", "no linker found";
    DEV_RUNNING = 5022, "DevRunning", "giong dev already serves this project";
    COMPILER_WARNING = 5023, "CompilerWarning", "the compiler has a warning";
    LISTENS_ON_NETWORK = 5024, "ListensOnNetwork", "the service would listen on every network interface";
}

impl Code {
    /// One of the runtime's codes (`GIONG-E0001`...), for a finding about it.
    pub const fn runtime(number: u16, summary: &'static str) -> Code {
        Code {
            number,
            name: "",
            summary,
        }
    }

    /// The number, such as 5000.
    #[allow(dead_code)] // read by giong ai and new (#217)
    pub fn number(self) -> u16 {
        self.number
    }

    /// The name, such as `Usage`; empty for a runtime code.
    #[allow(dead_code)] // read by giong ai and new (#217)
    pub fn name(self) -> &'static str {
        self.name
    }

    /// The one-line summary.
    #[allow(dead_code)] // read by giong ai and new (#217)
    pub fn summary(self) -> &'static str {
        self.summary
    }

    /// The page that explains it, for the version of this command.
    pub fn url(self) -> String {
        format!(
            "https://github.com/giong-run/Giong/blob/v{}/docs/errors/E{:04}.md",
            env!("CARGO_PKG_VERSION"),
            self.number
        )
    }

    /// Looks up a code of the command by its number.
    pub fn of(number: u16) -> Option<Code> {
        ALL.iter().copied().find(|c| c.number == number)
    }
}

impl fmt::Display for Code {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "GIONG-E{:04}", self.number)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes_are_in_range_sorted_and_unique() {
        for pair in ALL.windows(2) {
            assert!(pair[0].number < pair[1].number, "{} {}", pair[0], pair[1]);
        }
        for c in ALL {
            assert!((5000..=5099).contains(&c.number), "{c}");
            assert!(!c.name.is_empty() && !c.summary.is_empty());
            assert!(!c.summary.ends_with('.'), "{c}");
        }
        assert_eq!(Code::USAGE.to_string(), "GIONG-E5000");
        assert_eq!(Code::of(5000), Some(Code::USAGE));
        assert!(Code::USAGE.url().ends_with("/docs/errors/E5000.md"));
    }

    #[test]
    fn every_code_has_its_page_and_every_page_its_code() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../docs/errors");
        if !dir.exists() {
            return; // the published crate has no docs/
        }
        let index = std::fs::read_to_string(dir.join("README.md")).unwrap();
        for c in ALL {
            let file = format!("E{:04}.md", c.number);
            let page = std::fs::read_to_string(dir.join(&file))
                .unwrap_or_else(|e| panic!("{c} has no page docs/errors/{file}: {e}"));
            let title = format!("# {c}: {}", c.summary);
            assert_eq!(page.lines().next(), Some(title.as_str()), "{file}");
            let row = format!("| [{c}]({file}) | `{}` | {} |", c.name, c.summary);
            assert!(index.contains(&row), "docs/errors/README.md lacks {row}");
        }
        for entry in std::fs::read_dir(&dir).unwrap() {
            let name = entry.unwrap().file_name().into_string().unwrap();
            if let Some(n) = name.strip_prefix("E50").and_then(|r| r.strip_suffix(".md")) {
                let number: u16 = 5000 + n.parse::<u16>().unwrap();
                assert!(
                    Code::of(number).is_some(),
                    "docs/errors/{name} belongs to no code"
                );
            }
        }
    }
}
