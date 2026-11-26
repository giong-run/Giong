#!/usr/bin/env python3
"""Tests of scripts/ci-scope.py: what a pull request and a merge group run under
Workflow v2 (docs/WORKFLOW.md).

    python3 scripts/test_ci_scope.py
"""
import importlib.util
import tempfile
import unittest
from pathlib import Path

spec = importlib.util.spec_from_file_location("ci_scope", Path(__file__).with_name("ci-scope.py"))
scope = importlib.util.module_from_spec(spec)
spec.loader.exec_module(scope)


class Plan(unittest.TestCase):
    def plan(self, files, event="pull_request", draft=False, relevant=None):
        return scope.plan(files, event, draft, relevant)[0]

    def test_docs_on_a_pull_request(self):
        self.assertEqual(self.plan(["site/index.html", "changelog.d/123.md"]), "docs")

    def test_known_flakes_and_workflow_data_are_docs(self):
        self.assertEqual(self.plan([".github/known-flakes.txt"]), "docs")
        self.assertEqual(self.plan(["docs/workflow/data/v2/runs.json.gz"]), "docs")

    def test_code_on_a_pull_request_is_the_fast_set(self):
        self.assertEqual(self.plan(["src/net.rs"]), "pr")
        self.assertEqual(self.plan(["src/net.rs"], draft=True), "draft")

    def test_the_merge_queue_runs_everything_for_code(self):
        self.assertEqual(self.plan(["site/a.md", "src/net.rs"], event="merge_group"), "full")

    def test_a_docs_only_merge_group_runs_lint_and_msrv(self):
        self.assertEqual(self.plan(["site/a.md", "changelog.d/1.md"], event="merge_group"), "docs")

    def test_relevant_never_narrows_the_merge_queue(self):
        # The Windows workflow passes --relevant; in the queue a change outside those
        # paths is still code.
        self.assertEqual(self.plan(["giong-cli/src/main.rs"], event="merge_group",
                                   relevant=["src/"]), "full")

    def test_an_unknown_group_runs_everything(self):
        # No file list (the base could not be fetched): full.
        self.assertEqual(self.plan([], event="merge_group"), "full")
        self.assertEqual(self.plan([], event="workflow_dispatch"), "full")
        self.assertEqual(self.plan([], event="push"), "full")

    def test_code_prose_is_code(self):
        self.assertEqual(self.plan(["docs/errors/E0001.md"]), "pr")


class Platform(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.root = Path(self.tmp.name)

    def tearDown(self):
        self.tmp.cleanup()

    def write(self, path, text):
        p = self.root / path
        p.parent.mkdir(parents=True, exist_ok=True)
        p.write_text(text)
        return path

    def on(self, files, name):
        return scope.touches_platform(files, name, root=self.root)

    def test_the_platform_layer(self):
        self.assertTrue(self.on(["src/sys/unix.rs"], "macos"))
        self.assertTrue(self.on(["src/driver/iocp/alarm.rs"], "windows"))
        self.assertFalse(self.on(["src/driver/kqueue.rs"], "windows"))

    def test_a_cfg_naming_the_platform(self):
        mac = self.write("giong-tls/src/sys.rs", '#[cfg(target_os = "macos")]\nfn f() {}\n')
        win = self.write("giong-cli/src/host.rs", "#[cfg(windows)]\nfn f() {}\n")
        self.assertTrue(self.on([mac], "macos"))
        self.assertFalse(self.on([mac], "windows"))
        self.assertTrue(self.on([win], "windows"))
        self.assertFalse(self.on([win], "macos"))

    def test_prose_about_windows_is_not_windows(self):
        # HTTP/2 flow-control windows are not Microsoft Windows.
        f = self.write("giong-http/src/h2/flow.rs", "// the send and receive windows\nfn f() {}\n")
        self.assertFalse(self.on([f], "windows"))

    def test_a_manifest_runs_both(self):
        self.assertTrue(self.on(["giong-mysql/Cargo.toml"], "windows"))
        self.assertTrue(self.on(["Cargo.lock"], "macos"))

    def test_a_deleted_file_does_not_count(self):
        self.assertFalse(self.on(["giong-web/src/gone.rs"], "macos"))


if __name__ == "__main__":
    unittest.main()
