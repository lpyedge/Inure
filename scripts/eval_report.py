#!/usr/bin/env python3
"""从真实 RunRecord JSONL 统计 RIG 感知评估，不擅自宣布 G1 通过。

用法: python3 scripts/eval_report.py --input path/to/runs.jsonl [--format json]
Python 3.11+，无额外依赖。数据格式以 crates/rig-eval/src/metrics.rs 为准。
注意 RunRecord 目前没有端到端耗时、run_id/seed/运行环境字段，不能凭空补出这些指标。
"""

from __future__ import annotations

import argparse
from collections import defaultdict
import json
import math
from pathlib import Path
import sys
import tomllib
from typing import Any, Iterable

ROOT = Path(__file__).resolve().parent.parent
MODES = ("screenshot_baseline", "tree", "tree_image", "pixels_only", "ab_snapshot")
REQUIRED_G1_MODES = ("screenshot_baseline", "tree", "tree_image")
COUNTS = ("steps", "input_tokens", "output_tokens", "wrong_clicks", "stale_rejections")


class ReportError(ValueError):
    pass


def read_records(path: Path) -> list[dict[str, Any]]:
    records = []
    with path.open(encoding="utf-8") as fh:
        for lineno, raw in enumerate(fh, 1):
            if not raw.strip():
                continue
            try:
                r = json.loads(raw)
                if not isinstance(r, dict):
                    raise ReportError("一行必须是对象")
                for name in ("task_id", "model"):
                    if not isinstance(r.get(name), str) or not r[name]:
                        raise ReportError(f"{name} 必须是非空字符串")
                if r.get("mode") not in MODES:
                    raise ReportError("mode 非法")
                for name in ("success", "leaked_secret"):
                    if type(r.get(name)) is not bool:
                        raise ReportError(f"{name} 必须是 bool")
                for name in COUNTS:
                    if type(r.get(name)) is not int or r[name] < 0:
                        raise ReportError(f"{name} 必须是非负整数")
                lats = r.get("observe_latencies_ms")
                if not isinstance(lats, list) or any(type(v) is not int or v < 0 for v in lats):
                    raise ReportError("observe_latencies_ms 必须是非负整数数组")
                records.append(r)
            except (json.JSONDecodeError, ReportError, TypeError) as exc:
                raise ReportError(f"{path.name}:{lineno}: {exc}") from exc
    if not records:
        raise ReportError("JSONL 文件没有 RunRecord")
    return records


def load_task_tags(tasks_dir: Path) -> dict[str, list[str]]:
    tags: dict[str, list[str]] = {}
    for path in sorted(tasks_dir.glob("*.toml")):
        task = tomllib.loads(path.read_text(encoding="utf-8"))
        if not isinstance(task.get("id"), str) or task["id"] in tags:
            raise ReportError(f"重复或无效任务 ID: {path.name}")
        tags[task["id"]] = list(task.get("tags", []))
    if not tags:
        raise ReportError(f"任务目录为空: {tasks_dir}")
    return tags


def percentile(values: Iterable[int], p: float) -> int | None:
    xs = sorted(values)
    if not xs:
        return None
    rank = max(1, math.ceil((p / 100.0) * len(xs)))
    return xs[min(rank, len(xs)) - 1]


def summarize(records: list[dict[str, Any]]) -> dict[str, Any]:
    if not records:
        raise ReportError("不能汇总空列表")
    n = len(records)
    counts = {key: sum(r[key] for r in records) for key in COUNTS}
    lat = [v for r in records for v in r["observe_latencies_ms"]]
    return {
        "runs": n,
        "successes": sum(r["success"] for r in records),
        "success_rate": sum(r["success"] for r in records) / n,
        "mean_steps": counts["steps"] / n,
        "mean_input_tokens_per_run": counts["input_tokens"] / n,
        "mean_output_tokens_per_run": counts["output_tokens"] / n,
        "mean_total_tokens_per_run": (counts["input_tokens"] + counts["output_tokens"]) / n,
        "input_tokens_per_step": counts["input_tokens"] / counts["steps"] if counts["steps"] else 0.0,
        "wrong_clicks": counts["wrong_clicks"],
        "stale_rejections": counts["stale_rejections"],
        "p50_observe_ms": percentile(lat, 50),
        "p95_observe_ms": percentile(lat, 95),
        "leaks": sum(r["leaked_secret"] for r in records),
    }


def build_report(
    records: list[dict[str, Any]],
    task_tags: dict[str, list[str]],
    min_runs: int = 5,
    required_modes: tuple[str, ...] = REQUIRED_G1_MODES,
) -> dict[str, Any]:
    if min_runs < 1:
        raise ReportError("min_runs 必须 >= 1")
    unknown = sorted({r["task_id"] for r in records} - task_tags.keys())
    if unknown:
        raise ReportError("记录引用未知任务 ID: " + ", ".join(unknown))
    models = sorted({r["model"] for r in records})
    per_task: dict[tuple[str, str], list[dict[str, Any]]] = defaultdict(list)
    per_mode: dict[str, list[dict[str, Any]]] = defaultdict(list)
    per_tag: dict[tuple[str, str], list[dict[str, Any]]] = defaultdict(list)
    for r in records:
        per_task[(r["task_id"], r["mode"])].append(r)
        per_mode[r["mode"]].append(r)
        for tag in task_tags[r["task_id"]]:
            per_tag[(tag, r["mode"])].append(r)

    # G1 不含 no-semantics，其他任务每一种必需模式都需要足够样本。
    required_tasks = sorted(tid for tid, tags in task_tags.items() if "no-semantics" not in tags)
    missing = [f"{tid}/{mode}: {len(per_task[(tid, mode)])}/{min_runs}"
               for tid in required_tasks for mode in required_modes
               if len(per_task[(tid, mode)]) < min_runs]
    leaks = sum(r["leaked_secret"] for r in records)
    reasons = []
    if missing:
        reasons.append("样本不足: " + "; ".join(missing))
    if len(models) != 1:
        reasons.append("model 不一致，无法进行同模型比较")
    if leaks:
        reasons.append(f"至少 {leaks} 次运行标记了秘密值泄露")
    reasons.append("RunRecord 无端到端总耗时、配对实验编号与运行环境；必须由运行器另行提供证据")
    return {
        "data_ready_for_manual_g1_review": not missing and len(models) == 1 and leaks == 0,
        "g1_decision": "NOT_DETERMINED (负责人需确认阈值并核验原始运行证据)",
        "models": models,
        "total_runs": len(records),
        "min_runs_per_task_mode": min_runs,
        "reasons": reasons,
        "by_task_mode": [
            {"task_id": tid, "mode": mode, "tags": task_tags[tid], **summarize(rs)}
            for (tid, mode), rs in sorted(per_task.items()) if rs
        ],
        "by_mode": [{"mode": mode, **summarize(rs)} for mode, rs in sorted(per_mode.items())],
        "by_tag_mode": [
            {"tag": tag, "mode": mode, **summarize(rs)}
            for (tag, mode), rs in sorted(per_tag.items())
        ],
    }


def markdown(report: dict[str, Any]) -> str:
    lines = [
        "# RIG 感知评估数据汇总（自动统计，非 G1 决议）", "",
        f"- RunRecord 行数：{report['total_runs']}",
        f"- 模型：{', '.join(report['models'])}",
        f"- 达到最低样本数且无已标记泄密：{'是（待复核原始证据）' if report['data_ready_for_manual_g1_review'] else '否'}",
        f"- 决策门：{report['g1_decision']}", "",
        "## 按模式汇总", "",
        "| 模式 | 次数 | 成功率 | 平均总 Token/任务 | 输入 Token/步 | 平均步骤 | p95观察(ms) | 泄密 |", 
        "|---|---:|---:|---:|---:|---:|---:|---:|",
    ]
    for r in report["by_mode"]:
        p95 = "n/a" if r["p95_observe_ms"] is None else str(r["p95_observe_ms"])
        lines.append(f"| {r['mode']} | {r['runs']} | {r['success_rate']:.1%} | "
                     f"{r['mean_total_tokens_per_run']:.1f} | {r['input_tokens_per_step']:.1f} | "
                     f"{r['mean_steps']:.1f} | {p95} | {r['leaks']} |")
    lines += ["", "## 按任务/模式", "",
              "| 任务 | 模式 | 次数 | 成功率 | 平均总 Token/任务 | 误点 | 陈旧拒绝 | 泄密 |",
              "|---|---|---:|---:|---:|---:|---:|---:|"]
    for r in report["by_task_mode"]:
        lines.append(f"| {r['task_id']} | {r['mode']} | {r['runs']} | {r['success_rate']:.1%} | "
                     f"{r['mean_total_tokens_per_run']:.1f} | {r['wrong_clicks']} | {r['stale_rejections']} | {r['leaks']} |")
    lines += ["", "## 按标签/模式", "",
              "| 标签 | 模式 | 次数 | 成功率 | 平均总 Token/任务 |",
              "|---|---|---:|---:|---:|"]
    for r in report["by_tag_mode"]:
        lines.append(f"| {r['tag']} | {r['mode']} | {r['runs']} | {r['success_rate']:.1%} | {r['mean_total_tokens_per_run']:.1f} |")
    lines += ["", "## 证据缺口与待决", ""]
    lines += [f"- {r}" for r in report["reasons"]]
    lines += ["", "报告只统计 RunRecord 字段。没有验证模型调用真实性、输入截断、浏览器版本、视频同步、人工确认或动作实际执行；请补充原始日志与环境信息。", ""]
    return "\n".join(lines)


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--input", type=Path, required=True, help="实际运行产生的 RunRecord JSON Lines")
    parser.add_argument("--tasks-dir", type=Path, default=ROOT / "eval/tasks/web")
    parser.add_argument("--min-runs", type=int, default=5)
    parser.add_argument("--format", choices=["markdown", "json"], default="markdown")
    parser.add_argument("--output", type=Path, help="可选：将统计结果写入文件")
    args = parser.parse_args(argv)
    try:
        result = build_report(read_records(args.input), load_task_tags(args.tasks_dir), args.min_runs)
        data = json.dumps(result, ensure_ascii=False, indent=2) + "\n" if args.format == "json" else markdown(result)
        if args.output:
            args.output.parent.mkdir(parents=True, exist_ok=True)
            args.output.write_text(data, encoding="utf-8")
            print(f"汇总结果已写入 {args.output}")
        else:
            print(data)
        return 0
    except (OSError, ValueError, tomllib.TOMLDecodeError) as exc:
        print(f"评估报告生成失败: {exc}", file=sys.stderr)
        return 2


if __name__ == "__main__":
    sys.exit(main())
