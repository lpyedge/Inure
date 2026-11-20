# Edge Browser — Chromium 控制与 AI/人双通道

同机或局域网直连即完整形态：边缘程序 `rig-edge-browser` 直连 Chromium（CDP），
同时 serving AI 通道（观察/动作）与人通道（观看页/音视频流）。

```bash
cargo build
cargo test
python3 eval/verify_fixtures.py               # 用真实 Chromium 校验评估任务（需要 playwright）
python3 -m unittest discover -s tests/eval_report -v
```

本地构建缓存与 Python 缓存重定向（保持仓库干净）：

```bash
source scripts/local-env.sh   # CARGO_TARGET_DIR=.temp/target 等
```

## 目录

| 路径 | 内容 |
|---|---|
| `crates/rig-edge-browser` | 边缘程序：Chromium 拉起/挂靠（CDP）、AI 通道（HTTP/MCP）、人通道（观看页 `viewer.html/js`、RTC/HLS） |
| `crates/rig-proto` | 共享协议类型：`Observation`/`Action` 等 AI 通道契约 |
| `crates/rig-eval` | 感知层评估框架：任务定义、指标汇总、泄密检测 |
| `eval/` | 评估任务（`tasks/`）、测试页面（`fixtures/`）、历史运行证据（`results/`，只读） |
| `docs/` | 边缘程序、感知层、评估方法与历史结论 |
| `scripts/` | `eval_report.py`（RunRecord 汇总）、`local-env.sh`（本地缓存重定向）、`local-gate.sh`（本地等价 CI） |
| `.temp/` | 本地可再生输出：构建缓存、测量结果、日志（git 忽略，可随时删除） |

评估数字必须来自实际运行，注明环境；历史结论见 `docs/EVAL_REPORT_2026-11-18.md` 等。
网关/传输层相关代码已整仓移除（另有备份），本仓库只保留 Chromium 控制与 AI/人双通道。
