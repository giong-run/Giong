//! The codes the `giong` command reports: `GIONG-E5000` to `GIONG-E5099`.
//!
//! Ranges (one owner each, so work in parallel never collides):
//! E5000-E5029 the command itself, `doctor`, `check`, `explain`, `clean`, `mcp`, `dev`;
//! E5030-E5039 and E5080-E5089 `new`, `ai`, `fix`; E5040-E5079 `db`, config,
//! dashboard, `openapi`, `top`, `bench`, recording, `image`; E5090-E5099 free.
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
    NO_CONTAINER_RUNTIME = 5040, "NoContainerRuntime", "neither Docker nor Podman answers";
    DB_NOT_READY = 5041, "DbNotReady", "the database does not answer queries";
    DB_URL = 5042, "DbUrl", "no usable database URL";
    MIGRATION_CHANGED = 5043, "MigrationChanged", "an applied migration was changed";
    MIGRATION_FAILED = 5044, "MigrationFailed", "a migration failed";
    MIGRATION_NAME = 5045, "MigrationName", "a migration file is not valid";
    DB_CLIENT_MISSING = 5046, "DbClientMissing", "no database client to run the SQL";
    MIGRATION_UNKNOWN = 5047, "MigrationUnknown", "the database has a migration the project does not";
    DB_NOT_MANAGED = 5048, "DbNotManaged", "the database was not started by giong db up";
    DB_START_FAILED = 5049, "DbStartFailed", "the database container could not start";
    NO_SERVICE = 5050, "NoService", "no running service to ask";
    MCP_NO_TOOL = 5051, "McpNoTool", "the service's giong-mcp lacks a tool";
    MCP_TOOL_ERROR = 5052, "McpToolError", "the service's giong-mcp refused a call";
    WORKER_BLOCKED = 5055, "WorkerBlocked", "a worker of the service is stuck in one task";
    SERVICE_WARNING = 5056, "ServiceWarning", "the service reports a warning";
    BENCH_TARGET = 5060, "BenchTarget", "the service to measure cannot be reached";
    BENCH_AGAINST = 5061, "BenchAgainst", "the build to compare against cannot be made";
    BENCH_GENERATOR_BEHIND = 5062, "BenchGeneratorBehind", "the load generator fell behind its schedule";
    BENCH_ERRORS = 5063, "BenchErrors", "requests failed during the benchmark";
    BENCH_REGRESSION = 5064, "BenchRegression", "the change is slower than the build it is compared with";
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
    pub fn number(self) -> u16 {
        self.number
    }

    /// The name, such as `Usage`; empty for a runtime code.
    pub fn name(self) -> &'static str {
        self.name
    }

    /// The one-line summary.
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
