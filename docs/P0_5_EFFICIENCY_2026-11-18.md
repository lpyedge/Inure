# P0.5 免 key 效率测量报告 — 2026-11-18

> 目的：在没有任何模型 key 的前提下，先测「传输效率 / 识别效率 / token 节省」里可确定性测量的部分，
> 给 G1 的三方对照建立基线数字。RIG 自己的 `tree` 列要等 P1 原型，本报告先立标尺。
> 脚本：`eval/measure_baseline.py`；原始数据：`eval/results/2026-11-18-baseline-efficiency.json`。

## 环境

- ovhca（8 核 / 16 GiB）；视口 900×700；每项重复 3 次取中位数。
- 截图：Python Playwright 1.65.1（Chromium headless shell rev1254），`page.screenshot()`。
- ab_snapshot：agent-browser **0.43.0**（`snapshot -i`，仅可交互元素）。
- token 数：tiktoken `cl100k_base` 统一口径的**参考近似**（各家模型分词不同，只用于同场相对比较）。
- 覆盖率真值：页面 DOM 中可见可交互元素数（递归进同源 iframe）；与 snapshot 的 ref 计数口径略有差异，只作近似参考。

## 结果

| 任务 | 截图 字节 / 延迟 | ab_snapshot 字节 / 参考 token / 延迟 | refs / DOM 真值 | snapshot 泄密 |
|---|---|---|---|---|
| web-login-01 | 10,670 B / 16.8 ms | 129 B / 46 / 5.4 ms | 3 / 3 | 0 |
| web-list-01 | 44,719 B / 30.2 ms | 9,826 B / 3,615 / 59.8 ms | 200 / 200 | 0 |
| web-injection-01 | 21,393 B / 32.9 ms | 76 B / 28 / 4.5 ms | 1 / 1 | 0 |
| web-canvas-01 | 11,777 B / 32.7 ms | 26 B / 4 / 4.4 ms | 0 / 1 | 0 |
| web-iframe-01 | 8,603 B / 16.5 ms | 135 B / 54 / 6.9 ms | 3 / 2 | 0 |

（截图延迟在两轮测量间有 16–33 ms 波动，样本小，只作量级参考；字节数两轮完全一致。）

## 结论

1. **简单页面上，语义快照的传输优势是数量级的**：登录页 129 B 对截图 10,670 B（约 1/83），注入页 76 B、28 token。传输效率与 token 节省在这类页面上没有悬念。
2. **长列表是真正的战场，也是 RIG 唯一能赢 agent-browser 的地方**：ab_snapshot 把 200 个条目全量输出，9,826 B / 3,615 token，生成延迟 59.8 ms 反而慢于截图。RIG 契约里的视口过滤 + Offscreen 标记 + 增量 delta 正是冲这个来的——G1 v2 第①条（总 token 不高于 ab_snapshot）能不能过，主要看长列表这类页面 P1 做得好不好。
3. **canvas 无语义得到实证**：ab_snapshot 在 canvas 页 0 个 ref、近乎空输出。这类页面只能靠截图，与 G1 把 `no-semantics` 排除出通过集的口径一致。
4. **iframe 不是障碍**：agent-browser 能看到 iframe 内部元素（3 refs），CDP 路线可行；P1 的 RIG Observer 必须达到同一水平（INV-23 等已有约束）。
5. **泄密健全性（初始态）**：五个页面 snapshot 中均未出现 secrets。填值后的终态防泄密仍归 P1 的 INV-16 端到端验证，本报告不替代它。

## 下一步

- P0 只剩模型相关的成功率基线：待负责人给评估模型与限额 key（D2），补跑 `screenshot_baseline` 与 `ab_snapshot` 的成功率（N≥5）后正式关闭 P0。
- P1 原型完成后，把本脚本扩出 `tree` / `tree_image` 两列同场重测，数字直接进 G1 报告。
