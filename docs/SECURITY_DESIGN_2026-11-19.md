# 安全边界整改最小设计（待 LIU 批准）

> 来源：2026-11-19 GPT 对 `rig_review_20261119.zip` 的 review（编号 R-xx 沿用该报告），
> 经主 Muse 逐行核实属实。本文件按仓库 `AGENTS.md` 对 `requires_approval` 任务的要求，
> 只写**最小设计 / 风险 / 迁移与回滚 / 测试计划**，批准前不改协议、数据模型与安全边界。
> A 批正确性修复（R-05/08/09-decode/10/11/12/14/15/16/17）不在本文件范围，已另行实施。

## B1. R-02 目标页隔离（snapshot 会话范围）

**现状**：`snapshot.rs` 收集浏览器内全部 `page`/`iframe` 会话，无宿主的会话根追加到
观察顶层。自拉起模式下页面弹窗（window.open）的新页、挂靠模式下既有的其他标签页，
都会进入 AI 的观察与可操作范围。

**风险**：AI 获得从未被授权展示的页面内容（其他标签可能含邮箱、银行、私密会话），
且可对其执行动作。挂靠模式最严重：用户借出的是「一个浏览器」，预期隔离单位是标签页。

**最小设计**：
- 边缘在连接时确定**唯一授权目标页**（launch：自建的页；attach：连接时唯一的
  page target；若 attach 时存在多个 page target，默认拒绝启动并报错，
  或经 `--target-url-prefix` 显式指定其一——二选一，倾向前者，简单且 fail-closed）。
- snapshot 只采集授权页会话 + 其后代 iframe 会话（沿 `host_of` 父链可达者）；
  其余 page 会话一律不采集、不嫁接、不追加。
- 运行中授权页新开的弹窗页（opener 为授权页）默认**不**纳入；页面自身导航不算新页。
- 动作的 session 解析同步受限：locate 表只含授权页内的元素（采集范围决定）。

**迁移与回滚**：无数据迁移。行为变更：多标签内容不再出现于观察（这是预期收紧）。
回滚 = 还原采集过滤的一处改动。

**测试计划**：集成负例——测试页 window.open 弹窗含标记文本，断言观察文本与
Observation JSON 均不含该标记、且弹窗元素 id 不可 act；attach 模式预置双标签，
断言拒绝启动（或只观察指定页）。

## B2. R-03 Origin 生命周期校验

**现状**：`origin_allowed` 只在显式 `Action::Navigate` 时执行；点击触发的导航、
JS 跳转、服务端重定向、跨源 iframe 内容均无校验。origin 解析为手写字符串切分
（无默认端口规范化、无 userinfo 处理）。

**风险**：AI 被页面诱导（或按自身误判）点进白名单外站点后，后续观察会把该站
内容回给模型、动作继续生效——白名单只守住了「主动导航」一个入口。

**最小设计**：
- 引入 `url` crate 解析与规范化 origin（scheme/host/有效端口，默认端口归一），
  替换手写解析；userinfo 存在时按真实 host 判定。
- **观察侧**：observe 时读取主文档实际 URL，若主文档 origin 不在白名单，
  观察返回 PolicyDenied（不输出页面内容），并在结果中标明当前 origin 供调用方决策。
- **动作侧**：act 执行后复查主文档 origin；若已越界，立即停止后续动作并回
  PolicyDenied。跨源 iframe：其内容嫁接保留（同页语义需要），但在 Observation
  中标注各子帧 origin（协议已有 origin 字段则填实，无则列入协议变更评估）。
- 白名单语义不变：`--url` 的 origin + `--allow-origin` 追加。

**迁移与回滚**：新增 `url` 依赖（workspace 已有其他 crate 用它则复用版本）。
行为变更：越界后观察被拒——调用方需先 Navigate 回白名单内。回滚 = 移除
观察/动作后校验点（解析规范化建议保留）。

**测试计划**：集成负例——白名单页内点击跳转到第二 origin（本地双端口模拟），
断言跳转后 observe 被拒、act 被拒；默认端口（:80/:443）与带 userinfo 的
URL 解析正反例单测。

## B3. R-04 无令牌回环 WS 的 Origin 校验

**现状**：有令牌时 `auth_guard` 覆盖 `/ws`；无令牌回环模式下 `ws_handler`
只查通道闸门，不校验 Origin/Host。任意本地网页（浏览器内）理论上可尝试
连接 `ws://127.0.0.1:39871/ws` 收画面、送输入（受浏览器 Local Network Access
等政策制约，GPT 标注为「风险需联调」，非已证可利用）。

**最小设计**：WS 升级时校验：
- 带 `Origin` 头时：其 host 必须是回环（127.0.0.1/localhost/::1）且端口等于
  本服务监听端口（即只允许自家观看页）；不匹配 → 403。
- 无 `Origin` 头（非浏览器客户端，如脚本/测试）：放行（这类客户端不受
  CSWSH 威胁模型约束，且现有集成测试依赖此路径）。
- Host 头同理校验为回环监听地址。此校验在有无令牌时都生效（纵深防御）。

**迁移与回滚**：无迁移。回滚 = 移除升级前校验。

**测试计划**：集成负例——带 `Origin: http://evil.example` 升级应 403；
带本服务 Origin 应 101；无 Origin 应 101；现有 WS 测试不回归。

## B4. R-06 + R-07 Point 校验与帧原子性（合并设计）

**现状**：
- R-06：act 只对 `Target::Element` 做陈旧/边界校验；`Target::Point` 直接执行，
  proto 已有的 `check_point_target`（帧号 + 视口边界）运行时从未被调用。
- R-07：act 内部先调 observe() 推进帧号，再以「请求帧的 Observation（seen）+
  当前 LocateMap」拼凑校验；Observation 与 LocateMap 分锁更新，并发
  observe/act 可交错出不配套的（帧, 定位表）。

**最小设计**：
- AiChannel 内部状态合并为单个 `Mutex<FrameState>`，FrameState =
  `{ frame_id, observation, locate }` 不可变快照，observe 整体替换。
- act 流程改序：先取当前 FrameState → 对请求帧做校验（Element 走
  `check_element_target`、Point 走 `check_point_target`，均以**同一份**
  FrameState 为基准，帧号不等即 StaleFrame）→ 校验通过才执行 → 执行后
  observe 产生新帧。act 不再在校验前自调 observe。
- 同一 AiChannel 的 act 串行化（act 专用 Mutex），observe 可并发但只替换
  整体快照。HTTP 与 MCP 前端共用同一 AiChannel，语义一致。

**风险与代价**：act 语义变化——「以旧帧元素执行动作」的宽容度下降
（旧实现拿当前帧 locate 兜底执行，元素 id 若已漂移可能点错）。新语义与
协议契约（陈旧校验）一致，是收紧而非放宽。调用方纪律不变：先 observe
再以该帧 act。

**迁移与回滚**：无协议变更（check_point_target 已在 proto 中）。
回滚 = 还原 ai.rs 内部状态结构。

**测试计划**：单测/集成负例——旧帧 Point 点击应 StaleFrame；越界 Point
应被拒；旧帧 Element 在页面变化后应 StaleFrame（已有）；并发 observe/act
压力下不出现「帧与 locate 不配套」错误（断言 act 成功时命中的元素与
该帧观察一致）。

## B5. R-01（RIG-R02）数据库跨租户复合约束迁移

**现状**：`migrations/0001_init.sql` 只有基础 FK；GPT 复现 10 类非法组合
（跨 Tenant 的 Resource/Target/Grant/Session/Lease 关联）均可入库。
`scripts/ai_review.py check --profile known` 稳定 FAIL RIG-R02。

**最小设计**（方向已在 2026-11-18 LIU 批准 R01 修复时确立的「约束下沉到
数据库」口径内细化）：
- 相关表补复合唯一键（如 `resources(id, tenant_id)` UNIQUE），子表外键改
  复合 FK 引用 `(resource_id, tenant_id)` 等，一致性由数据库强制，
  不靠应用层检查。
- 具体 10 类非法组合逐条映射到约束点，写入迁移的验收清单。

**迁移与回滚**：新增 `migrations/0002_*.sql`，不改 0001；迁移前对现存
数据跑一致性探针（当前为开发数据，预期零冲突；探针非空则停下报告）。
回滚 = 反向迁移脚本（删复合 FK/唯一键），与正向迁移成对提交。

**测试计划**：`ai_review.py check --profile known` 中 RIG-R02 的 10 类
反例全部由「可入库」变「被拒绝」；正例（同租户正常关联链）保持可入库；
迁移在空库与含正例数据的库上各跑一遍。

## 批准方式

逐项批准即可（B1–B5 相互独立，可分批实施）。批准后按「设计 → 负例测试
先行 → 实现 → local-gate → 提交」推进，每项单独提交、单独汇报。

## LIU 裁定（2026-11-19）

LIU 的統一口徑：RIG 是個人／受信環境、人工監督的工具，不是多租戶不可信
部署；GPT review 的 P0 按後者的威脅模型定級，按本項目的實際模型重新裁定。

- **B1 撤銷**：不做目標頁隔離。隔離靠 AI 通道閘門本身——AI 不接入、或
  接入不處理，人工需要協助時才真實開始處理。多頁觀察只受識別與傳輸
  速度的自然限制，不做人為限制。
- **B2 撤銷**：不做 Origin 生命週期校驗（原 v1 已有的「顯式 Navigate
  白名單」保留，屬 G1 定調）。理由：此類校驗防君子不防小人，真正的
  存取控制是 token。
- **B3 撤銷**：不做 WS Origin 校驗，理由同 B2；非回環綁定必須設
  token 的既有強制不變。已知取捨：Origin 唯一能擋的是瀏覽器內惡意
  網頁的跨站連接（瀏覽器不許其偽造 Origin），在無 token 回環的
  受信場景明確接受此風險。
- **B5 撤銷**：不做跨租戶複合約束遷移。部署模型改為單 profile；
  Gateway 授權模型＝人工確認後簽發 token，AI 憑 token 接入配合
  操作。RIG-R02 檢查不改檢查器遷就，在審查狀態中記錄為「業主已
  明確 de-scope 多租戶」。
- **B4 保留待確認**：經重新解釋（不是限制 AI，而是 AI 不得拿過期／
  錯位的畫面資訊動手——AI 所見須與人工所見一致、動作須符合人工
  預期），等 LIU 確認後實施；輸入仲裁已在「取消接管」裁定中確認，
  歸入 B4 一併落。
- **B4 補充（2026-11-19 晚）**：B4 中的「輸入仲裁」由 LIU 定的人機
  交互模型取代——AI 待命期只連通不處理（省 token），執行指令期間
  WebUI 遮蔽人工輸入並顯示 AI 特殊指針，用戶隨時可強制接管中斷
  （邊緣停注入 + 中斷結果 + `control_taken_over` 通知）。全文規範
  見 `docs/EDGE_BROWSER.md` §7.1–7.3。B4 餘下兩項（Point 坐標
  校驗、幀原子性）仍待 LIU 按新解釋確認。
