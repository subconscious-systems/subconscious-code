import contextlib
from copy import deepcopy
import io
import json
import os
from pathlib import Path
import sys
import tempfile
import time
import unittest
from unittest.mock import patch

from evaluations import run


class SuiteTests(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory()
        self.addCleanup(self.directory.cleanup)
        self.path = Path(self.directory.name) / "suite.json"
        self.suite = json.loads((run.ROOT / "evaluations/tasks.json").read_text())

    def load(self):
        self.path.write_text(json.dumps(self.suite), encoding="utf-8")
        return run.load_suite(self.path)

    def test_real_suite_has_five_distinct_pinned_cases(self):
        suite = run.load_suite(run.ROOT / "evaluations/tasks.json")
        self.assertEqual(len(suite["tasks"]), 5)
        self.assertEqual(len({task["id"] for task in suite["tasks"]}), 5)

    def test_duplicate_ids_are_rejected(self):
        self.suite["tasks"].append(deepcopy(self.suite["tasks"][0]))
        with self.assertRaises(ValueError):
            self.load()

    def test_unpinned_and_escaping_inputs_are_rejected(self):
        for value in ["main", "deadbeef", "../secret"]:
            self.suite["source_revision"] = value
            with self.assertRaises(ValueError):
                self.load()
        self.suite["source_revision"] = "a" * 40
        self.suite["grader"] = "../outside.rs"
        with self.assertRaises(ValueError):
            self.load()

    def test_oracle_cannot_rewrite_a_manifest(self):
        self.suite["tasks"][0]["oracle_files"] = ["Cargo.toml"]
        with self.assertRaises(ValueError):
            self.load()

    def test_zero_and_ignored_acceptance_sets_are_not_allowed(self):
        self.suite["tasks"][0]["goal_tests"] = 0
        with self.assertRaises(ValueError):
            self.load()

    def test_malformed_types_fail_with_controlled_validation_errors(self):
        for suite in [[], {"schema_version": True}, {"schema_version": 1, "suite_id": None}]:
            self.path.write_text(json.dumps(suite), encoding="utf-8")
            with self.assertRaises(ValueError):
                run.load_suite(self.path)
        self.suite["tasks"][0]["editable"] = "not-an-array"
        with self.assertRaises(ValueError):
            self.load()


class GradingTests(unittest.TestCase):
    def result(self, log, code=0, timeout=False):
        return run.ProcessResult(code, timeout, 1, log)

    def test_clean_process_is_not_proof_of_passed_tests(self):
        result = run.parse_grade(self.result("I fixed the task"), 1)
        self.assertEqual(result["status"], "grade_error")

    def test_zero_tests_and_ignored_tests_do_not_count_as_success(self):
        for receipt in ["test result: ok. 0 passed; 0 failed; 0 ignored;",
                        "test result: ok. 0 passed; 0 failed; 1 ignored;"]:
            self.assertEqual(run.parse_grade(self.result(receipt), 1)["status"], "grade_error")

    def test_correct_receipt_and_process_status_are_both_required(self):
        log = "test result: ok. 1 passed; 0 failed; 0 ignored;"
        self.assertEqual(run.parse_grade(self.result(log), 1)["status"], "resolved")
        self.assertEqual(run.parse_grade(self.result(log, 1), 1)["status"], "unresolved")

    def test_failed_assertions_are_unresolved_not_infrastructure_errors(self):
        log = "test result: FAILED. 0 passed; 1 failed; 0 ignored;"
        self.assertEqual(run.parse_grade(self.result(log, 101), 1)["status"], "unresolved")

    def test_timeouts_and_compile_errors_remain_distinct(self):
        self.assertEqual(run.parse_grade(self.result("", -1, True), 1)["status"], "grade_error")
        self.assertEqual(run.parse_grade(self.result("error[E0308]: mismatched types", 101), 1)["status"], "unresolved")

    def test_invalid_changes_are_detected_even_when_deleted(self):
        changes = run.changed_files({"Cargo.toml": "a", "crates/rc-tools/src/glob.rs": "a"}, {"crates/rc-tools/src/glob.rs": "b"})
        task = {"editable": ["crates/rc-tools/src/*.rs", "crates/rc-tools/tests/*.rs"]}
        self.assertEqual(run.forbidden_changes(changes, task), ["Cargo.toml"])
        grader = f"crates/rc-tools/tests/{run.GRADE_TARGET}.rs"
        self.assertEqual(run.forbidden_changes([grader], task), [grader])

    def test_partial_runs_do_not_publish_a_complete_solve_rate(self):
        records = [{"task_status": "resolved", "runtime_clean": False, "agent_wall_ms": 10}]
        summary = run.summarize(records, 5)
        self.assertIsNone(summary["resolved_rate"])
        self.assertFalse(summary["complete"])
        summary = run.summarize(records, 1)
        self.assertEqual(summary["resolved_rate"], 1)
        self.assertEqual(summary["runtime_clean_trials"], 0)


class ProcessTests(unittest.TestCase):
    def test_credentials_are_redacted_across_every_chunk_boundary(self):
        secret = "private-test-credential"
        message = ("prefix " + secret + " suffix").encode()
        for cut in range(1, len(message)):
            capture = run.Capture(secret)
            capture.feed(message[:cut])
            capture.feed(message[cut:])
            text = capture.text()
            self.assertNotIn(secret, text)
            self.assertIn("[credential redacted]", text)
            self.assertIn("prefix", text)
            self.assertIn("suffix", text)

    def test_capture_is_bounded_and_retains_the_end(self):
        capture = run.Capture(limit=100)
        for _ in range(100):
            capture.feed(b"abcde" * 20)
        capture.feed(b"FINAL RESULT")
        text = capture.text()
        self.assertTrue(text.endswith("FINAL RESULT"))
        self.assertLess(len(text), 150)

    def test_environment_does_not_copy_provider_secrets_or_proxies(self):
        with patch.dict(os.environ, {"SC_API_KEY": "secret", "GH_TOKEN": "secret", "HTTPS_PROXY": "secret"}):
            env = run.minimal_env()
        self.assertNotIn("SC_API_KEY", env)
        self.assertNotIn("GH_TOKEN", env)
        self.assertNotIn("HTTPS_PROXY", env)

    @unittest.skipUnless(os.name == "nt", "Windows compiler environment")
    def test_windows_compiler_setup_preserves_a_real_linker(self):
        import shutil
        with tempfile.TemporaryDirectory() as directory:
            env = run.compiler_env(run.minimal_env(Path(directory)))
        self.assertIsNotNone(shutil.which("link.exe", path=env["PATH"]))
        self.assertTrue(env["LIB"])
        self.assertNotIn("SC_API_KEY", env)

    def test_subprocess_output_and_timeout_are_observed(self):
        with tempfile.TemporaryDirectory() as directory:
            result = run.execute([sys.executable, "-c", "print('READY')"], Path(directory), run.minimal_env(), 10)
            self.assertEqual(result.returncode, 0)
            self.assertIn("READY", result.log)
            result = run.execute([sys.executable, "-c", "import time; time.sleep(30)"], Path(directory), run.minimal_env(), 1)
            self.assertTrue(result.timed_out)
            self.assertNotEqual(result.returncode, 0)

    def test_background_descendants_cannot_hold_capture_open(self):
        script = "import subprocess,sys; subprocess.Popen([sys.executable,'-c','import time;time.sleep(30)']); print('PARENT EXIT')"
        started = time.monotonic()
        with tempfile.TemporaryDirectory() as directory:
            result = run.execute([sys.executable, "-c", script], Path(directory), run.minimal_env(), 10)
        self.assertEqual(result.returncode, 0)
        self.assertIn("PARENT EXIT", result.log)
        self.assertLess(time.monotonic() - started, 10)


class InputAndArtifactTests(unittest.TestCase):
    def test_urls_do_not_accept_embedded_credentials(self):
        for url in ["https://user:secret@example.com/v1", "https://example.com/v1?key=secret", "ftp://example.com", "https://example.com/#secret"]:
            with self.assertRaises(ValueError):
                run.endpoint(url)
        self.assertEqual(run.endpoint("https://example.com/v1/"), "https://example.com/v1")

    def test_accounting_copies_only_safe_numeric_fields(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "report.json"
            path.write_text(json.dumps({"schema_version": 1, "outcome": "stop", "api_key": "secret", "request_count": 3,
                                        "usage": {"input_tokens": 12, "total_tokens": True, "secret": "value"}}))
            data = run.read_accounting(path)
            self.assertEqual(data["request_count"], 3)
            self.assertEqual(data["usage"], {"input_tokens": 12})
            self.assertNotIn("secret", json.dumps(data))
            path.write_text("{broken")
            self.assertEqual(run.read_accounting(path)["report_status"], "invalid")

    def test_inventory_rejects_links(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / "original").write_text("data")
            try:
                (root / "alias").symlink_to(root / "original")
            except OSError:
                self.skipTest("symlink creation unavailable")
            with self.assertRaises(ValueError):
                run.inventory(root)

    def test_missing_live_configuration_never_creates_output(self):
        with tempfile.TemporaryDirectory() as directory:
            out = Path(directory) / "should-not-exist"
            with patch.dict(os.environ, {}, clear=True), contextlib.redirect_stderr(io.StringIO()):
                status = run.main(["run", "--binary", sys.executable, "--model", "test", "--base-url", "http://localhost/v1", "--out", str(out)])
            self.assertEqual(status, 1)
            self.assertFalse(out.exists())


if __name__ == "__main__":
    unittest.main()
