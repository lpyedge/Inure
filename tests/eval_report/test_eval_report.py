"""评估统计器的数学与数据校验自测。测试数据全部为虚构合成数据。"""

import json
from pathlib import Path
import sys
from tempfile import TemporaryDirectory
import unittest

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / "scripts"))

import eval_report  # noqa: E402


def record(task="login", mode="tree", success=True, steps=2, inputs=200, outputs=50, lats=None, leaked=False):
    return {
        "task_id": task, "mode": mode, "model": "synthetic-test-model", "success": success,
        "steps": steps, "input_tokens": inputs, "output_tokens": outputs,
        "observe_latencies_ms": [10, 20] if lats is None else lats,
        "wrong_clicks": 0, "stale_rejections": 0, "leaked_secret": leaked,
    }


class EvalReportTests(unittest.TestCase):
    def test_total_cost_and_latency_are_per_task_not_per_step(self):
        result = eval_report.summarize([
            record(steps=2, inputs=200, outputs=50, lats=[10, 20]),
            record(success=False, steps=4, inputs=600, outputs=150, lats=[30, 100]),
        ])
        self.assertEqual(result["runs"], 2)
        self.assertEqual(result["success_rate"], 0.5)
        self.assertEqual(result["mean_total_tokens_per_run"], 500.0)
        self.assertAlmostEqual(result["input_tokens_per_step"], 800 / 6)
        self.assertEqual(result["p50_observe_ms"], 20)
        self.assertEqual(result["p95_observe_ms"], 100)

    def test_g1_data_readiness_requires_coverage_not_only_global_count(self):
        tags = {"login": ["secret"], "canvas": ["no-semantics"]}
        many_baselines_only = [record(mode="screenshot_baseline") for _ in range(20)]
        report = eval_report.build_report(many_baselines_only, tags, min_runs=5)
        self.assertFalse(report["data_ready_for_manual_g1_review"])
        self.assertEqual(report["g1_decision"].split()[0], "NOT_DETERMINED")
        enough = [record(mode=m) for m in eval_report.REQUIRED_G1_MODES for _ in range(5)]
        report = eval_report.build_report(enough, tags, min_runs=5)
        self.assertTrue(report["data_ready_for_manual_g1_review"])
        self.assertEqual(report["g1_decision"].split()[0], "NOT_DETERMINED")

    def test_leak_and_mixed_models_prevent_ready(self):
        tags = {"login": ["secret"]}
        enough = [record(mode=m) for m in eval_report.REQUIRED_G1_MODES for _ in range(5)]
        enough[0]["leaked_secret"] = True
        self.assertFalse(eval_report.build_report(enough, tags)["data_ready_for_manual_g1_review"])
        enough[0]["leaked_secret"] = False
        enough[0]["model"] = "different-model"
        self.assertFalse(eval_report.build_report(enough, tags)["data_ready_for_manual_g1_review"])

    def test_jsonl_io_rejects_incorrect_types_and_unknown_mode(self):
        with TemporaryDirectory() as tmp:
            p = Path(tmp) / "runs.jsonl"
            a = record()
            p.write_text(json.dumps(a) + "\n", encoding="utf-8")
            self.assertEqual([a], eval_report.read_records(p))
            a["mode"] = "nonexistent"
            p.write_text(json.dumps(a) + "\n", encoding="utf-8")
            with self.assertRaises(eval_report.ReportError):
                eval_report.read_records(p)
            a["mode"] = "tree"
            a["steps"] = True  # bool 不能冒充整数
            p.write_text(json.dumps(a) + "\n", encoding="utf-8")
            with self.assertRaises(eval_report.ReportError):
                eval_report.read_records(p)

    def test_unknown_task_is_rejected(self):
        with self.assertRaises(eval_report.ReportError):
            eval_report.build_report([record(task="unknown")], {"login": ["secret"]})

    def test_real_task_tag_manifest_loads(self):
        tags = eval_report.load_task_tags(ROOT / "eval/tasks/web")
        self.assertIn("web-login-01", tags)
        self.assertIn("secret", tags["web-login-01"])
        self.assertIn("no-semantics", tags["web-canvas-01"])


if __name__ == "__main__":
    unittest.main()
