//! Builds `tests/suite.rs`'s modules: Gióng's own integration tests, each
//! test function wrapped so that it runs inside the simulation.
//!
//! For every file in SUITE, the test functions lose their `#[test]` and
//! become `pub`; a wrapper with the same attributes (`#[ignore]`,
//! `#[should_panic]`) runs the original in `giong_sim::run_test`.

use std::fmt::Write as _;
use std::path::Path;

/// The test files of the root crate that run on the simulation, and the
/// tests left out with the reason (see sim/README.md).
const SUITE: &[&str] = &[
    "cancel", "channels", "executor", "multi", "net", "shutdown", "time", "udp", "unix", "wake",
    "spin", "blocked", "observe", "fs", "balance",
];

/// Tests that cannot run on the simulation, and why.
const SKIP: &[(&str, &str)] = &[
    (
        "blocked::a_runtime_that_requires_io_uring_says_why_it_cannot_start",
        "io_uring only",
    ),
    (
        "blocked::a_kernel_that_refuses_buffer_rings_is_named_not_called_old",
        "io_uring only",
    ),
    (
        "blocked::the_default_runtime_falls_back_to_epoll_and_works",
        "checks the real kernel's io_uring",
    ),
    (
        "blocked::a_used_up_locked_memory_limit_is_named_and_falls_back_to_epoll",
        "io_uring only",
    ),
    (
        "cancel::dropping_an_in_flight_send_with_control_keeps_its_buffers_alive",
        "control messages are not simulated",
    ),
    (
        "net::send_with_control_hands_the_control_message_to_the_kernel",
        "control messages are not simulated",
    ),
    (
        "multi::sigterm_triggers_a_graceful_shutdown",
        "signals are not simulated",
    ),
    (
        "multi::sigterm_still_works_after_worker_0_returned",
        "signals are not simulated",
    ),
    (
        "multi::sigterm_delivered_to_a_thread_outside_the_runtime_shuts_it_down",
        "signals are not simulated",
    ),
    (
        "multi::a_second_signal_ends_a_process_whose_shutdown_hangs",
        "signals are not simulated",
    ),
    (
        "udp::dropping_a_socket_closes_it",
        "inspects the real kernel's descriptor table",
    ),
    (
        "fs::a_read_that_never_completes_can_be_cancelled",
        "a FIFO blocks in the real kernel",
    ),
    (
        "cancel::a_dropped_close_still_closes_on_the_epoll_driver",
        "a FIFO blocks in the real kernel",
    ),
    (
        "fs::a_stuck_file_operation_does_not_hold_up_the_others",
        "a FIFO blocks in the real kernel",
    ),
    (
        "fs::a_file_is_closed_only_once_a_blocking_thread_is_done_with_it",
        "a FIFO blocks in the real kernel",
    ),
    ("fs::standard_input_pipes_stay_blocking", "real pipes"),
    (
        "fs::metadata_describes_files_directories_and_links",
        "compares the virtual wall clock with a real file's time",
    ),
];

fn main() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("tests");
    // The copies sit in a `tests/` directory, as the originals do: some
    // tests check that a location's file ends with `tests/<name>.rs`.
    let out = Path::new(&std::env::var("OUT_DIR").unwrap())
        .join("suite")
        .join("tests");
    std::fs::create_dir_all(&out).unwrap();
    println!("cargo:rerun-if-changed=build.rs");
    let mut index = String::new();
    for name in SUITE {
        let path = root.join(format!("{name}.rs"));
        println!("cargo:rerun-if-changed={}", path.display());
        let src = std::fs::read_to_string(&path).unwrap();
        let common = root.join("common").join("mod.rs");
        let mut body = String::new();
        let mut wrappers = String::new();
        let mut pending: Vec<String> = Vec::new();
        let mut in_test = false;
        for line in src.lines() {
            let t = line.trim_start();
            if line == "mod common;" {
                writeln!(
                    body,
                    "#[path = {:?}]\nmod common;",
                    common.display().to_string()
                )
                .unwrap();
                continue;
            }
            if line == "#[test]" {
                in_test = true;
                pending.clear();
                continue;
            }
            if in_test
                && (t.starts_with("#[ignore") || t.starts_with("#[should_panic"))
                && !line.starts_with(' ')
            {
                pending.push(line.to_string());
                continue;
            }
            if in_test && line.starts_with("fn ") {
                let fname = line[3..].split('(').next().unwrap();
                writeln!(body, "#[allow(dead_code)]\npub {line}").unwrap();
                let should_panic = pending.iter().any(|a| a.contains("should_panic"));
                let attrs: Vec<&String> = pending.iter().collect();
                let skip = SKIP.iter().find(|(t, _)| *t == format!("{name}::{fname}"));
                let ignore = skip
                    .map(|(_, why)| format!("#[ignore = {why:?}]"))
                    .unwrap_or_default();
                let mut attrs: Vec<String> = attrs
                    .into_iter()
                    .filter(|a| skip.is_none() || !a.contains("ignore"))
                    .cloned()
                    .collect();
                attrs.insert(0, "#[test]".to_string());
                if !ignore.is_empty() {
                    attrs.push(ignore);
                }
                writeln!(
                    wrappers,
                    "{}\nfn {fname}() {{ giong_sim::run_test(\"{name}::{fname}\", {should_panic}, super::{name}::{fname}) }}",
                    attrs.join("\n")
                )
                .unwrap();
                in_test = false;
                continue;
            }
            if in_test {
                // Another attribute between #[test] and fn: keep it.
                pending.push(line.to_string());
                continue;
            }
            writeln!(body, "{line}").unwrap();
        }
        std::fs::write(out.join(format!("{name}.rs")), body).unwrap();
        writeln!(
            index,
            "#[allow(unused_imports, clippy::all)]\n#[path = {:?}]\nmod {name};\nmod sim_{name} {{\n{wrappers}}}\n",
            out.join(format!("{name}.rs")).display().to_string()
        )
        .unwrap();
    }
    std::fs::write(out.join("index.rs"), index).unwrap();
}
