# 安全整改与接入方案（2026-11-22）

来源：外部审查意见（基于 `3786c0e`）经逐条对照代码核实后，与 LIU 确认的改动清单。
状态列：待办 / 进行中 / 完成。每项先写失败测试，再改实现。

## 一、直接修复（不涉及决策）

| # | 问题 | 位置 | 修法 | 状态 |
|---|---|---|---|---|
| 1 | 反序完成后状态卡在 Executing，人输入被一直遮蔽 | `exec.rs` holdover | 保持窗改用「在途数归零序号」判定，不再比对 call 开始代次 | 完成 |
| 2 | `paste_image` 把 WS 字符串拼进页面 JS，可注入 | `human.rs` | 校验 base64 字符集与大小，并以参数传入 | 完成 |
| 3 | 空字符串 token 通过启动检查 | `main.rs` | 空 token 视同未设 | 完成 |
| 4 | `Select`/`Upload` 写入前无接管检查 | `ai.rs` | 实际 CDP 写入前补 `live(h)` | 完成 |
| 5 | DOM 模式剪贴板复制结果广播给所有 Viewer | `human.rs` | 只回给请求端 | 完成 |
| 6 | `Type` 点击后仍用点击前快照的焦点会话 | `ai.rs` | 插入与 submit 前复核实时焦点属于预期目标 | 完成 |
| 7 | MCP `initialize` 原样回显客户端协议版本 | `main.rs` | 只协商已测版本 | 完成 |

## 二、认证与路由拆分（安全边界变更，LIU 已批准）

### 路由拆分
- **Viewer 路由**：`/`、`/viewer.js`、`POST /session`、`/ws`、`/channels`、`/print.pdf`、`/stream/*`。需 `--human`。
- **AI 路由**：`/observe`、`/act`、`/screenshot.png`。仅纯 HTTP 模式挂载。
- `--stdio --human`：只挂 Viewer 路由，AI 只能走 MCP，本机进程无法经 HTTP 绕过 AI 闸门。
- 纯 HTTP 局域网模式：两组路由同端口。

### 启动参数
- `--token`（由启动程序随机生成并传入）：起任何 HTTP 监听或 viewer 时必填，缺失或为空拒绝启动。`--profile-dir`、`--cache-dir` 保持可选（未给时仍用一次性临时目录）。
- 非回环监听必须 TLS，否则拒绝启动。
- 随机替换 = 启动程序换值重启。

### 令牌机制
- 启动 token 只用于换票：`POST /session`，token 放 body，不放 URL。
- 换得 `access`（短效）+ `refresh`（无时限，每次使用轮换）。
- 请求头 `Authorization: Bearer <access>`；WebSocket 无法带头，连上后首帧发 `{type:"auth"}`，3 秒内未认证即断开。
- viewer 在 access 将过期时用 refresh 静默续期，无登录时长。
- 票据只存内存，重启即全部失效，无需吊销接口。
- 主 token 可直接调 AI 路由；Viewer 票据不能调 AI 路由。

### 两种打开方式
- **独立打开**：显示 token 输入页，提交后走换票流程。
- **内嵌 iframe**：宿主 `postMessage({type:"rig-auth", token})`，viewer 校验 `--embed-origin` 白名单；服务端用同一白名单设 `frame-ancestors`。未配置则禁止任何内嵌。
- https 宿主不能内嵌 http viewer（混合内容）；自签证书需用户先信任一次。

### HLS 媒体流（已落实）
- HLS 由原生 `<video src>` 请求，无法带头：改用只读、仅限 `/stream` 的随机路径钥匙 `stream_key`（登录后下发），
  是「令牌不入 URL」的唯一例外，不具备控制能力。

### 文档
- 接入说明（含 postMessage 协议）写入 `docs/EDGE_BROWSER.md` 一节，不单开新文件。

| # | 改动 | 状态 |
|---|---|---|
| 8 | 路由拆分 + stdio 只挂 Viewer 路由 | 完成 |
| 9 | 必填参数与非回环强制 TLS | 完成 |
| 10 | `/session` 换票与 access/refresh 机制 | 完成 |
| 11 | `viewer.js`：输入页、`fch`/WS 改头与首帧认证、postMessage 接入 | 完成 |
| 12 | `--embed-origin` 与 `frame-ancestors` | 完成 |
| 13 | EDGE_BROWSER.md 接入章节 | 完成 |

## 三、Upload（保留能力，限定边界）

- 上传目录为 `<cache-dir>/uploads/`；未给 `--cache-dir` 时用进程级临时目录 `<tmp>/rig-edge-<pid>-uploads`，不另设参数。
- 未专门指定的目录（临时 profile、临时上传目录）在 Chromium 关闭后于退出时清理；指定的目录不动。SIGKILL/崩溃无法兜底。
- 路径 `canonicalize` 后必须落在该目录内，符号链接逃逸一律拒绝。
- 人工要给 AI 传文件，把文件放进该目录。

| # | 改动 | 状态 |
|---|---|---|
| 14 | Upload 路径限定到 cache-dir 子目录 | 完成 |

## 四、接管（保持现状，小补强）

- 「接管后 observe 解锁」是既定口径，**不改**为人工显式交还。
- 已有 AI 指针 `ghost` 与强制接管，满足需求。
- 补强：AI 恢复控制时 viewer 显示「AI 已恢复控制」提示；MCP 接管结果附加文本块「请停止原任务并等待用户新指示」（不改 `rig-proto` 契约）。

| # | 改动 | 状态 |
|---|---|---|
| 15 | 恢复提示与 Aborted 文案 | 完成 |

## 五、不在本次范围

- 外部审查附带的 OpenClaw/Hermes 配置与体积数字：来源不是本仓库实测，未经实际运行不写入文档。
- `act` schema 是否声明 `request_id`：未核实，暂不处理。
- DOM 镜像传原始表单值：登录场景视为可接触凭证的通道，已知，不在本轮改动。
