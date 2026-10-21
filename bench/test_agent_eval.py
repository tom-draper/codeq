import json
import hashlib
from pathlib import Path
import tempfile
import unittest
import subprocess
import runpy

import agent_eval
from agent_eval_tasks import TaskSuite
import generic_tools_mcp


class EvaluationTests(unittest.TestCase):
    def test_answer_accepts_the_requested_json_integer(self):
        task = {"kind": "answer", "expected_answer": 17, "last_text": '{"answer":17}'}
        self.assertTrue(agent_eval.validate(task, None)["passed"])
        task["last_text"] = '{"answer":true}'
        self.assertFalse(agent_eval.validate(task, None)["passed"])

    def test_equivalent_edit_passes_without_a_required_source_expression(self):
        with tempfile.TemporaryDirectory() as temp:
            workspace = agent_eval.prepare_workspace(Path(temp) / "workspace")
            (workspace / "pricing.py").write_text(
                'def sum_prices(items):\n'
                '    return sum(row["price"] for row in items if not row.get("cancelled"))\n'
            )
            result = agent_eval.validate(
                {"kind": "edit", "last_text": '{"status":"done"}'}, workspace
            )
            self.assertTrue(result["passed"], result)
            self.assertEqual(result["changed_files"], ["pricing.py"])

    def test_wrong_edit_and_changed_tests_fail(self):
        with tempfile.TemporaryDirectory() as temp:
            workspace = agent_eval.prepare_workspace(Path(temp) / "workspace")
            task = {"kind": "edit", "last_text": '{"status":"done"}'}
            self.assertFalse(agent_eval.validate(task, workspace)["passed"])
            (workspace / "pricing.py").write_text('def sum_prices(items):\n    return 5\n')
            (workspace / "test_pricing.py").write_text('')
            result = agent_eval.validate(task, workspace)
            self.assertFalse(result["passed"], result)

    def test_usage_does_not_add_message_totals_to_stream_totals(self):
        events = [
            {"type": "message_start", "message": {"usage": {"input_tokens": 10}}},
            {"type": "message_delta", "usage": {"output_tokens": 3}},
            {"type": "assistant", "message": {
                "usage": {"input_tokens": 10, "output_tokens": 3}, "content": []}},
            {"type": "result", "result": '{"answer":17}',
             "usage": {"input_tokens": 10, "output_tokens": 3}},
        ]
        parsed = agent_eval.parse_events("\n".join(json.dumps(e) for e in events))
        self.assertEqual((parsed["input_tokens"], parsed["output_tokens"]), (10, 3))

    def test_generic_tools_skip_runtime_caches(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            (root / "source.py").write_text("needle\n")
            for directory in (".git", ".codeq", "__pycache__"):
                (root / directory).mkdir()
                (root / directory / "noise").write_text("needle\n")
            self.assertEqual(generic_tools_mcp.call(root, "list_files", {}), "source.py")
            self.assertEqual(generic_tools_mcp.call(root, "search_text", {"query": "needle"}),
                             "source.py:1:needle")

    def test_stream_deltas_are_cumulative_per_message(self):
        events = [
            {"type": "message_start", "message": {"usage": {"input_tokens": 10}}},
            {"type": "message_delta", "usage": {"output_tokens": 3}},
            {"type": "message_delta", "usage": {"output_tokens": 7}},
            {"type": "message_start", "message": {"usage": {"input_tokens": 20}}},
            {"type": "message_delta", "usage": {"output_tokens": 2}},
        ]
        parsed = agent_eval.parse_events("\n".join(json.dumps(e) for e in events))
        self.assertEqual((parsed["input_tokens"], parsed["output_tokens"]), (30, 9))

    def test_rename_oracle_rejects_deleting_existing_tests(self):
        checker = runpy.run_path(str(Path(__file__).parent / "agent_eval_tasks/check_rename.py"))["check"]
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            (root / "tests").mkdir()
            (root / "tests/test_utils.py").write_text('_split_content_type_header = None\n')
            baseline = root / "baseline.json"
            baseline.write_text(json.dumps({"tests/test_utils.py":
                'def test_parser():\n    assert _parse_content_type_header("x") == ("x", {})\n'}))
            with self.assertRaisesRegex(AssertionError, "existing tests changed"):
                checker(root, baseline)


class RepositoryTaskTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.repo = self.root / "corpus" / "sample"
        self.repo.mkdir(parents=True)
        (self.repo / "source.py").write_text("value = 1\n")
        self.git("init", "-q")
        self.git("add", ".")
        self.git("-c", "user.name=test", "-c", "user.email=test@example.com",
                 "-c", "commit.gpgsign=false", "commit", "-qm", "fixture")
        commit = self.git("rev-parse", "HEAD").strip()
        self.manifest = self.root / "tasks.json"
        self.spec = {"version": 1, "tasks": [{
            "id": "answer-value", "kind": "answer", "repo": "sample", "commit": commit,
            "prompt": "What is value?", "oracle": {"expected": 1, "evidence": {
                "source.py": hashlib.sha256(b"value = 1\n").hexdigest()}}
        }]}
        self.manifest.write_text(json.dumps(self.spec))

    def git(self, *args):
        return subprocess.check_output(["git", "-C", str(self.repo), *args], text=True)

    def suite(self):
        return TaskSuite(self.manifest, self.root / "corpus")

    def test_preparation_uses_pinned_source_and_isolates_profiles(self):
        (self.repo / "source.py").write_text("value = 99\n")
        (self.repo / "untracked.py").write_text("secret = 2\n")
        suite = self.suite()
        first = suite.prepare(suite.tasks[0], self.root / "first")
        second = suite.prepare(suite.tasks[0], self.root / "second")
        self.assertEqual((first.workspace / "source.py").read_text(), "value = 1\n")
        self.assertEqual(sorted(first.pristine), ["source.py"])
        self.assertTrue(first.validate('{"answer":1}')["passed"])
        (first.workspace / "source.py").write_text("value = 2\n")
        self.assertFalse(first.validate('{"answer":1}')["passed"])
        self.assertTrue(second.validate('{"answer":1}')["passed"])
        self.assertEqual(first.source_sha256, second.source_sha256)

    def test_stale_evidence_and_escaping_paths_are_rejected(self):
        self.spec["tasks"][0]["oracle"]["evidence"]["source.py"] = "0" * 64
        self.manifest.write_text(json.dumps(self.spec))
        suite = self.suite()
        with self.assertRaisesRegex(ValueError, "stale answer evidence"):
            suite.prepare(suite.tasks[0], self.root / "bad")
        self.spec["tasks"][0]["oracle"]["evidence"] = {"../secret": "0" * 64}
        self.manifest.write_text(json.dumps(self.spec))
        with self.assertRaisesRegex(ValueError, "escapes"):
            self.suite()

    def test_wrong_answer_fails_and_runtime_caches_do_not_count_as_edits(self):
        suite = self.suite()
        prepared = suite.prepare(suite.tasks[0], self.root / "workspace")
        (prepared.workspace / ".codeq").mkdir()
        (prepared.workspace / ".codeq" / "index.db").write_bytes(b"generated cache")
        self.assertTrue(prepared.validate('{"answer":1}')["passed"])
        self.assertFalse(prepared.validate('{"answer":true}')["passed"])
        self.assertFalse(prepared.validate('{"answer":2}')["passed"])


if __name__ == "__main__":
    unittest.main()
