"""Optional real-binary orchestration smoke; the provider is scripted offline."""
import contextlib
import http.server
import io
import json
import os
from pathlib import Path
from types import SimpleNamespace
import tempfile
import threading
import unittest
from unittest.mock import patch

from evaluations import run


@unittest.skipUnless(os.environ.get("SC_TEST_BINARY"), "set SC_TEST_BINARY to exercise the real CLI offline")
class PipelineSmoke(unittest.TestCase):
    def test_real_cli_patch_is_saved_and_graded_after_the_agent_exits(self):
        requests = []
        # Use a known local patch only to exercise orchestration. Rust grader
        # correctness is calibrated separately against all five corpus cases.
        target = "crates/rc-tools/src/glob.rs"
        content = (run.ROOT / target).read_text(encoding="utf-8")
        content = content.replace("match globset::Glob::new(&inp.pattern)",
                                  "match globset::GlobBuilder::new(&inp.pattern).literal_separator(true).build()")

        class Provider(http.server.BaseHTTPRequestHandler):
            def do_POST(self):
                body = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
                requests.append(body)
                self.send_response(200)
                self.send_header("Content-Type", "text/event-stream")
                self.end_headers()
                if len(requests) == 1:
                    delta = {"tool_calls": [{"index": 0, "id": "offline-write", "type": "function",
                                             "function": {"name": "Write", "arguments": json.dumps({"file_path": target, "content": content})}}]}
                    finish = "tool_calls"
                else:
                    delta, finish = {"content": "Offline scripted workflow completed."}, "stop"
                chunk = {"id": "offline-pipeline", "object": "chat.completion.chunk", "created": 1,
                         "model": "offline-scripted-provider", "choices": [{"index": 0, "delta": delta, "finish_reason": finish}]}
                self.wfile.write(("data: " + json.dumps(chunk) + "\n\ndata: [DONE]\n\n").encode())

            def log_message(self, *_):
                pass

        server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Provider)
        thread = threading.Thread(target=server.serve_forever, daemon=True)
        thread.start()
        self.addCleanup(server.server_close)
        self.addCleanup(server.shutdown)
        with tempfile.TemporaryDirectory() as directory:
            evaluator = run.Evaluator(run.ROOT, run.ROOT / "evaluations/tasks.json", Path(directory) / "artifacts", 30)
            self.addCleanup(evaluator.owned_workspace.cleanup)
            args = SimpleNamespace(binary=Path(os.environ["SC_TEST_BINARY"]).resolve(), model="offline-scripted-provider",
                                   base_url=f"http://127.0.0.1:{server.server_port}/v1", max_tokens=2048,
                                   max_iters=8, seconds=20, reasoning_effort="off", trials=1)

            def grade(task, output):
                patched = "literal_separator(true)" in (evaluator.workspace / target).read_text(encoding="utf-8")
                return {"task_status": "resolved" if patched else "unresolved",
                        "checks": {"preserve": {"status": "resolved"}, "goal": {"status": "resolved" if patched else "unresolved"}}}

            with patch.object(evaluator, "grade", side_effect=grade), contextlib.redirect_stdout(io.StringIO()):
                records = evaluator.live([evaluator.suite["tasks"][0]], args, "dummy-offline-pipeline-key")
            result = records[0]
            self.assertEqual(result["task_status"], "resolved")
            self.assertTrue(result["runtime_clean"])
            self.assertEqual(result["changed_files"], [target])
            self.assertTrue((evaluator.output / "glob_segments-1/candidate" / target).is_file())
            summary = json.loads((evaluator.output / "summary.json").read_text())
            self.assertTrue(summary["complete"])
            self.assertEqual(requests[0]["max_tokens"], 2048)
            self.assertEqual(len(requests), 3)
            self.assertNotIn("dummy-offline-pipeline-key", (evaluator.output / "glob_segments-1/agent.log").read_text())


if __name__ == "__main__":
    unittest.main()
