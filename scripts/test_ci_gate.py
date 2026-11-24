#!/usr/bin/env python3
"""Tests of scripts/ci-gate.py's decisions, on jobs shaped like the GitHub API's answers.
The cases come from the merge-queue runs of release 1.4.1 on 2026-11-23.

    python3 scripts/test_ci_gate.py
"""
import importlib.util
import re
import unittest
from pathlib import Path

spec = importlib.util.spec_from_file_location("ci_gate", Path(__file__).with_name("ci-gate.py"))
gate = importlib.util.module_from_spec(spec)
spec.loader.exec_module(gate)

ID = iter(range(1, 10_000))


def job(name, conclusion="failure", failed=None, cancelled=None, steps=True):
    """A job whose step `failed` failed (or `cancelled` was stopped), after checkout."""
    out = {"id": next(ID), "name": name, "conclusion": conclusion, "steps": []}
    if not steps:
        return out
    out["steps"].append({"name": "Set up job", "conclusion": "success"})
    out["steps"].append({"name": "Run actions/checkout@3d3c42e5aac5ba805825da76410c181273ba90b1", "conclusion": "success"})
    if failed:
        out["steps"].append({"name": failed, "conclusion": "failure"})
    if cancelled:
        out["steps"].append({"name": cancelled, "conclusion": "cancelled"})
    return out


ALL_CHECKS = job("all-checks", failed="Run jq -r 'to_entries[] | \"\\(.key): \\(.value.result)\"' <<<\"$NEEDS\"")
TSAN_AGG = job("tsan", failed='Run test "failure" = success || { test "success" = success; }')

# The libtest output of job "tsan (epoll, run 1)" of run 45165377766, as GitHub keeps it.
TSAN_LOG = """\
2026-11-23T11:38:24.9216340Z running 76 tests
2026-11-23T11:38:35.7689489Z pool::a_connection_goes_back_closed_while_a_cancel_from_its_lease_is_on_its_way --- FAILED
2026-11-23T11:38:39.1285575Z failures:
2026-11-23T11:38:39.1285727Z
2026-11-23T11:38:39.1286091Z ---- pool::a_connection_goes_back_closed_while_a_cancel_from_its_lease_is_on_its_way stdout ----
2026-11-23T11:38:39.1286964Z thread 'pool::a_connection...' (12131) panicked at giong-mysql/tests/mysql/pool.rs:869:9:
2026-11-23T11:38:39.1288892Z
2026-11-23T11:38:39.1289040Z failures:
2026-11-23T11:38:39.1289376Z     pool::a_connection_goes_back_closed_while_a_cancel_from_its_lease_is_on_its_way
2026-11-23T11:38:39.1289611Z
2026-11-23T11:38:39.1289862Z test result: FAILED. 75 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out
"""
FLAKES = [(355, re.compile("pool::a_connection_goes_back_closed_while_a_cancel_from_its_lease_is_on_its_way"))]


def decide(jobs, attempt=1, annotations=None, logs=None, flakes=(), open_issues=(355,)):
    annotations = annotations or {}
    logs = logs or {}
    return gate.decide(jobs, attempt,
                       lambda j: annotations.get(j["name"], []),
                       lambda j: logs.get(j["name"], ""),
                       list(flakes), lambda n: n in open_issues)


class Decisions(unittest.TestCase):
    def test_a_reset_mirror_reruns_only_that_job(self):
        # Run 45165377766: curl to archive.ubuntu.com reset, in what is now a net: step.
        verdict, why, rows = decide([job("servers (mysql 8.4)", failed="net: set up mysql 8.4"), ALL_CHECKS])
        self.assertEqual(verdict, "rerun", why)
        self.assertEqual([r[1] for r in rows], ["infra", "needs"])

    def test_a_failing_test_fails_the_gate(self):
        verdict, _, _ = decide([job("test", failed="Run ulimit -l && scripts/check.sh test"), ALL_CHECKS])
        self.assertEqual(verdict, "fail")

    def test_a_test_beside_a_network_failure_still_fails(self):
        verdict, _, _ = decide([
            job("servers (mysql 8.4)", failed="net: set up mysql 8.4"),
            job("miri", failed="Run scripts/check.sh miri"),
            ALL_CHECKS,
        ])
        self.assertEqual(verdict, "fail")

    def test_a_failed_download_beside_a_failed_test_in_one_job_fails(self):
        # The release build is fetched with continue-on-error: its failure must not make
        # the test that failed after it look like the network's fault.
        j = job("test", failed="net: fetch the release build")
        j["steps"].append({"name": "Run ulimit -l && scripts/check.sh test", "conclusion": "failure"})
        verdict, _, _ = decide([j, ALL_CHECKS])
        self.assertEqual(verdict, "fail")

    def test_a_hang_in_apt_reruns(self):
        # Run 45168187322: apt-get update hung until the job's time limit.
        verdict, why, _ = decide(
            [job("musl", conclusion="cancelled", cancelled="net: install musl-tools"), ALL_CHECKS],
            annotations={"musl": ["The job has exceeded the maximum execution time of 30m0s",
                                  "The operation was canceled."]})
        self.assertEqual(verdict, "rerun", why)

    def test_a_hang_in_a_test_fails(self):
        verdict, why, _ = decide(
            [job("test", conclusion="cancelled", cancelled="Run scripts/check.sh test"), ALL_CHECKS],
            annotations={"test": ["The job has exceeded the maximum execution time of 60m0s"]})
        self.assertEqual(verdict, "fail")
        self.assertIn("hang", decide(
            [job("test", conclusion="cancelled", cancelled="Run scripts/check.sh test")],
            annotations={"test": ["The job has exceeded the maximum execution time of 60m0s"]})[2][0][2])

    def test_a_lost_runner_reruns(self):
        verdict, _, _ = decide(
            [job("kernels (6.1.155)", cancelled="Run scripts/kernels.sh 6.1.155"), ALL_CHECKS],
            annotations={"kernels (6.1.155)": ["The runner has received a shutdown signal."]})
        self.assertEqual(verdict, "rerun")

    def test_a_job_that_never_started_reruns(self):
        verdict, _, _ = decide([job("instr", steps=False), ALL_CHECKS])
        self.assertEqual(verdict, "rerun")

    def test_a_job_stopped_for_no_known_reason_fails(self):
        verdict, _, _ = decide([job("instr", conclusion="cancelled", cancelled="Run scripts/instr.py check"), ALL_CHECKS],
                               annotations={"instr": ["The operation was canceled."]})
        self.assertEqual(verdict, "fail")

    def test_reruns_stop_after_three_attempts(self):
        jobs = [job("servers (mysql 8.4)", failed="net: set up mysql 8.4"), ALL_CHECKS]
        self.assertEqual(decide(jobs, attempt=2)[0], "rerun")
        self.assertEqual(decide(jobs, attempt=3)[0], "fail")

    def test_only_aggregates_failing_fails(self):
        self.assertEqual(decide([ALL_CHECKS, TSAN_AGG])[0], "fail")

    def test_setup_actions_count_as_outside_the_code(self):
        for step in ("Run dtolnay/rust-toolchain@683b588c487a69d01f6c6ddf40f036ff682cc2c8",
                     "Run actions/checkout@3d3c42e5aac5ba805825da76410c181273ba90b1",
                     "Run actions/upload-artifact@cf430e030ddbb5b0abf93d22962f4752f3646cd9"):
            j = job("build")
            j["steps"].append({"name": step, "conclusion": "failure"})
            self.assertEqual(decide([j, ALL_CHECKS])[0], "rerun", step)

    def test_the_test_job_waiting_on_a_failed_build_follows_the_build(self):
        # `build` failed in its network step; `test` fails its first step because of it.
        verdict, _, rows = decide([
            job("build", failed="Run actions/upload-artifact@cf430e030ddbb5b0abf93d22962f4752f3646cd9"),
            job("test", failed="The plan and the build must have succeeded"),
            ALL_CHECKS,
        ])
        self.assertEqual(verdict, "rerun")
        self.assertEqual(rows[1][1], "needs")


class KnownFlakes(unittest.TestCase):
    def tsan_run(self):
        return [job("tsan (epoll, run 1)", failed="ThreadSanitizer on epoll"), TSAN_AGG, ALL_CHECKS]

    def test_names_come_from_the_libtest_summary(self):
        self.assertEqual(gate.failed_tests(TSAN_LOG),
                         ["pool::a_connection_goes_back_closed_while_a_cancel_from_its_lease_is_on_its_way"])

    def test_a_listed_flake_with_an_open_issue_reruns_once(self):
        verdict, why, rows = decide(self.tsan_run(), logs={"tsan (epoll, run 1)": TSAN_LOG}, flakes=FLAKES)
        self.assertEqual(verdict, "rerun", why)
        self.assertIn("#355", why)
        self.assertEqual(rows[0][1], "flake")
        # Not a second time.
        self.assertEqual(decide(self.tsan_run(), attempt=2, logs={"tsan (epoll, run 1)": TSAN_LOG},
                                flakes=FLAKES)[0], "fail")

    def test_a_flake_whose_issue_is_closed_fails(self):
        self.assertEqual(decide(self.tsan_run(), logs={"tsan (epoll, run 1)": TSAN_LOG}, flakes=FLAKES,
                                open_issues=())[0], "fail")

    def test_an_unlisted_failure_fails(self):
        self.assertEqual(decide(self.tsan_run(), logs={"tsan (epoll, run 1)": TSAN_LOG})[0], "fail")

    def test_a_listed_flake_beside_another_failure_fails(self):
        log = TSAN_LOG.replace("    pool::a_connection_goes_back_closed_while_a_cancel_from_its_lease_is_on_its_way",
                               "    pool::a_connection_goes_back_closed_while_a_cancel_from_its_lease_is_on_its_way\n"
                               "2026-11-23T11:38:39.1289377Z     pool::another_test")
        self.assertEqual(gate.failed_tests(log)[-1], "pool::another_test")
        self.assertEqual(decide(self.tsan_run(), logs={"tsan (epoll, run 1)": log}, flakes=FLAKES)[0], "fail")

    def test_a_failure_without_a_test_name_fails(self):
        # A ThreadSanitizer report or a crash names no test.
        log = "2026-11-23T11:38:39Z WARNING: ThreadSanitizer: data race (pid=1)\n"
        self.assertEqual(decide(self.tsan_run(), logs={"tsan (epoll, run 1)": log}, flakes=FLAKES)[0], "fail")

    def test_the_repository_list_parses(self):
        flakes = gate.read_flakes(Path(__file__).resolve().parent.parent / ".github/known-flakes.txt")
        for issue, rx in flakes:
            self.assertGreater(issue, 0)
            self.assertIsInstance(rx, re.Pattern)


if __name__ == "__main__":
    unittest.main()
