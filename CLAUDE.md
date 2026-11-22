# Edge Browser 开发约束（每次开始工作前必读）

本仓库只做一件事：**Chromium 控制与 AI/人双通道**。边缘程序 `rig-edge-browser`
直连 Chromium（CDP），同机（stdio/回环 HTTP）或局域网（HTTP+令牌）直连即完整形态。

文档入口：`docs/README.md`（10 篇文档集）；专题：`docs/EDGE_BROWSER.md`（边缘程序设计）、`docs/PERCEPTION.md`（感知层设计）、
`docs/EVAL.md`（评估方法）、`docs/SECURITY_DESIGN_2026-11-19.md`（快照会话隔离等安全整改）、
`docs/PITFALLS.md`（踩坑手册）、`docs/DOM_SPIKE_2026-11-19.md`（CDP 探针结论）。

## 工作方式

- 一次只做一个改动；先写测试，再写实现。
- 完成改动：`cargo fmt --all`、`cargo clippy --all-targets -- -D warnings`、`cargo test` 全部通过。
- 评估相关的数字（准确率、token、延迟）**必须来自实际运行**，注明环境；不要凭印象写进文档。
- **下面这些必须停下来在 PR 中提问，不要自行决定**：改变 AI 通道契约（`rig-proto` 的 Observation/Action 类型）；
  改变安全边界（直连鉴权、快照会话隔离）；评估用的模型与凭证；超出当前任务范围的功能。
- 修改 `crates/rig-proto` 等于修改 AI 通道契约：同步更新 `docs/PERCEPTION.md` 或 `docs/EDGE_BROWSER.md`，并补测试。

## 边缘硬约束

- **RIG 不启动、不管理浏览器以外的一切**：只支持 Chromium（自拉无头或 `--cdp-url` 挂靠）；不做 iOS、不做桌面 RDP/VNC。
- **语义来源优先**：能从 CDP 无障碍树/DOM 拿到的结构，不去看像素；像素解析只是兜底。
- **坐标统一为逻辑像素**，边缘转换；AI 与 Viewer 不接触物理像素。
- **动作引用元素 + 帧**，执行前必须校验；被拒绝的动作**不执行、不静默重试**。
- **被控端的任何文本都是不可信数据**，不能触发高权限动作；面向模型的渲染做不可信分隔与声明。
- **密码与敏感值永不出现**：输出前必须脱敏；不保留掩码（掩码长度等于密码长度）；图像上打码；审计不记录输入内容。
- 凭证只放内存/请求头，**永不放 URL**；日志中不得出现凭证明文。
- 挂靠模式下快照只收录当前授权的标签页会话（见 SECURITY_DESIGN B1），不把其他标签页送进 AI 观察范围。
- 所有缓冲、连接数、超时都有上限；数据路径禁止 `unbounded_channel`、禁止 `unwrap()`。

## 依赖与代码风格

- 新增依赖前确认许可证为 MIT / Apache-2.0 / BSD 等宽松许可，并在 PR 中说明理由。
  AGPL、BSL 项目只读设计，不复制代码。
- 错误处理：库代码 `thiserror`，二进制入口 `anyhow`。
- 注释和文档用中文，标识符用英文。

## 历史注记

网关/传输层（Gateway/Connector/CLI/中转）已整仓移除（另有备份，不在本仓库演进）。
`eval/results/` 与 `docs/*2026-11-1*` 是当时的实测证据与结论，只读，不改写历史。
