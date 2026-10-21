import json
from pathlib import Path
import tempfile
import unittest

import agent_eval


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


if __name__ == "__main__":
    unittest.main()
