# 仓库级 AI 开发规则

> 适用于本地编码 Agent（Codex / Claude Code / Cursor 等）。本文件只提供统一入口，**不取代**项目已批准的 `CLAUDE.md` 和 `docs/` 决策。
> 范围：Chromium 控制与 AI/人双通道（`rig-edge-browser` + `rig-proto` + 评估体系）。网关/传输层已移除（另有备份）。

## 每次开始时的读取顺序

1. `AGENTS.md`、`README.md`、`CLAUDE.md`。
2. 与本次任务相关的 `docs/EDGE_BROWSER.md` / `docs/PERCEPTION.md` / `docs/EVAL.md` /
   `docs/SECURITY_DESIGN_2026-11-19.md` / `docs/PITFALLS.md`。
3. 当前任务对应的代码与测试；无需整仓重写。

## 推荐首轮命令（仓库根目录）

```bash
source scripts/local-env.sh
cargo fmt --all --check
cargo clippy --all-targets -- -D warnings
cargo test --all
python3 -m unittest discover -s tests/eval_report -p 'test_*.py' -v
```

- `cargo test` 全绿表示已实现部分通过；`eval/verify_fixtures.py` 需要真实 Chromium + playwright，按需运行。
- 只运行本地安全操作；不接入生产、不运行破坏性操作、不上传凭证。敏感值不得进入日志、报告与模型输入。

## 本地 AI 接单规则

- 一次只做一个改动；测试先行。不要删除/弱化已有断言。
- `requires_approval` 事项（改 AI 通道契约、改安全边界、评估模型与凭证、超范围功能）**先写最小设计方案，不直接改**，等负责人批准。
- 无必要不引入新组件、依赖或复杂抽象；M1 时代"不碰中转"那套规则已随网关移除而失效，不要再引用。
- 每次提交前，能运行时执行：`cargo fmt --all --check`、`cargo clippy --all-targets -- -D warnings`、
  `cargo test --all`。没安装 Rust/浏览器依赖时，报告"未验证"，不要臆造 PASS。
- 交付一份简明的证据记录：修改文件、命令与退出码、正/反例、剩余风险。

## 项目协作辅助文件

- `docs/EDGE_BROWSER.md`：边缘程序设计与双通道说明。
- `docs/PERCEPTION.md`：感知层设计（语义优先、坐标、动作校验）。
- `docs/EVAL.md`：评估方法；`scripts/eval_report.py`：RunRecord 汇总（不自动下结论）。
- `eval/`：评估任务、测试页面、历史运行证据（只读）。
