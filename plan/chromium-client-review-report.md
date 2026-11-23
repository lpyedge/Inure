# Chromium Client 项目代码审查与性能优化报告

**审查日期**：2026-11-24  
**审查对象**：用户上传的 `chromium-client(1).zip`（Rust workspace，`rig-edge-browser`、`rig-proto`、`rig-eval` 等）  
**方式**：Dev Expert Team 技术向（架构 TL、技术 TE），挂载 QA/SRE 视角；**同一主体静态自查，不是四位独立审查者**。代码未修改、未提交、未连接生产。  
**证据口径**：`源码已确认` 指代码路径客观存在；`条件性风险` 指仅在特定异常/负载下出现，尚未在实际服务中重现；`性能假设` 指必须测量后才能确认收益。

## 1. 执行摘要

当前项目已实现双通道：AI 经 CDP/AX/DOM 操作语义化页面，人通道按需求提供 JPEG、WebRTC、HLS 与 DOM 镜像。建议保留当前架构，优先改善**坐标准确性、长时运行的资源生命周期、采集任务异常恢复**，再按实际 p95 与 CPU/内存数据优化帧复制和观察链路。

**已完成、不要重复开发的措施**：帧泵无消费者时停止、RTC DataChannel 队列上限、DOM 差分上限、DOM `byId` 删除清理、部分 watcher 的停止清理、HLS 按播放列表请求懒启动与 12s 空闲回收、HLS 单 rendition 开关、行动前校验和截图敏感遮蔽。旧 `plan/RIG-問題清單-2026-11-20.md` 的问题不能直接视为现存。

## 2. 排序问题清单

### F01 | P1 | 深层 OOPIF 坐标合成最多 4 轮

- **代码证据**：`crates/rig-edge-browser/src/snapshot.rs:342-381` 使用 `for _ in 0..4` 计算子会话坐标偏移；`snapshot.rs:383-388` 不检查 `placed` 仍对会话建树；`snapshot.rs:85-105` 的 `frame_scope` 则支持任意深度。
- **触发条件**：跨进程 iframe 嵌套大于四层，且会话枚举顺序为子先父时，最深层在四轮后仍 `placed=false`，`offset` 维持 `(0,0)`；其元素的坐标和可见性可能错。
- **证据等级**：源码已确认；独立的顺序模拟（5 层逆序）四轮只能放置前四层；**未通过 Rust/Chromium 复现真实错点**。
- **最小修法**：传播轮数改为最多 `sessions.len()` 且达到不再前进时退出；对于未定位会话显式警告并**不应输出可点击的错误坐标**。该部分涉及安全/动作语义，改动前回归跨源 iframe、坐标点击与动态移除/重排。
- **回归测试**：至少五层 OOPIF、逆序 session、坐标非零并有偏移；父级隐藏/拆除时不错误点击。

### F02 | P1 | 帧泵异常停止后缺乏独立唤醒与重启

- **代码证据**：`crates/rig-edge-browser/src/human.rs:576-619` 的管理循环在启动成功/失败后只 `await request.changed()`；`task.is_finished()` 仅在下次循环才判断。
- **影响**：持续有观看者但泵提前退出，或首次启动暂时失败，在观看需求不变的情况下缺少重试触发条件；可能持续黑屏/旧画面，直到需求变化。
- **证据等级**：源码控制流已确认；具体真实发生率待压测。
- **最小修法**：`tokio::select!` 同时等待需求变化与泵终止，失败采用有上限的指数退避；加超时/失败次数和 `last_frame_age_ms` 健康条件。避免瞬时失败无限快速自旋。
- **回归测试**：启动 CDP 拒绝一次后恢复、任务退出但仍有订阅者、无订阅者时绝不重启、关闭通道时退出并清理。

### F03 | P1 | DOM 镜像被移除的同源 iframe 监视对象残留

- **代码证据**：`crates/rig-edge-browser/src/dom.rs:158-180` 的 `S.watched` 以 Document 作强引用 key，内含 MutationObserver/事件监听；`dom.rs:300-309,328-330` 删除 DOM 子树仅清理节点 ID 和 canvas；监视器仅在 iframe `load` 或 `stop` 时解除，删除 iframe 不保证触发 `load`。
- **影响**：单个长开 SPA 在反复增加/移除同源 iframe 后，过期的 Document、监听器和观察器可能逐步堆积。
- **证据等级**：源码中缺少移除清理路径已确认；内存斜率待量测。
- **最小修法**：记录 iframe 元素到 Document 清理闭包的映射；在 `dropSubtree` 里包括后代 iframe 执行断开、删除；对已断开元素提供兜底 sweep；保留 stop 时总清理。
- **回归测试**：单页创建/移除 1000 次同源 iframe，观测 `S.watched.size` 回落和 JS heap/RSS 稳定；覆盖 iframe 重新导航和嵌套。

### F04 | P1 条件性 | HLS 停止超时后可能与下一次启动共用输出目录

- **代码证据**：`crates/rig-edge-browser/src/media.rs:53-55` 目录固定 `rig-edge-hls-{pid}`；`media.rs:141-154` 等待合流线程最多 10s，超时仍执行 `remove_dir_all`；随后 `media.rs:89-112` 下一次启动在同一路径创建媒体线程。
- **影响**：在阻塞的采集/编码/IO 超过 10s 的异常场景，旧线程可能仍存活并触碰该目录，与新管线出现竞争、文件混杂和资源残留。
- **证据等级**：条件性并发风险，未重现；线程在正常 stop 后退出的路径已经实现。
- **最小修法**：每次管线使用唯一 generation 目录；把老目录的清理绑定到真实线程退出，而不是仅绑定超时返回；明确线程超时后不再被视为已完全释放。
- **回归测试**：人为让合流线程停机阻塞 >10s，立即重复 start/stop，检查路径隔离、线程数和最终删除。

### F05 | P1 | HLS 写包错误被忽略

- **代码证据**：`crates/rig-edge-browser/src/media.rs:309-314` 中 `let _ = p.write_interleaved(&mut self.out)`；`media.rs:489-508` 没有向管线状态传播写包失败。`main.rs:832-835` 的运行态可能仍为 true。
- **影响**：磁盘满、输出路径失效或 FFmpeg mux 写失败时，管线可能仍宣称运行中，而播放列表/分片停滞。
- **证据等级**：错误未传播为源码已确认；实际磁盘失败表现待故障注入。
- **最小修法**：`Rendition::write` 返回 `Result`，将错误写入 `media.last_error`/监控并结束或受控恢复管线；禁止无限连续重试。
- **回归测试**：只读/满磁盘故障、目录被删除，确认错误状态和回退行为。

### F06 | P2 | AI 元素 ID 映射只按 session 回收

- **代码证据**：`crates/rig-edge-browser/src/snapshot.rs:16-40` 的 `IdMap::id_for` 只新增键；`retain_sessions` 只删除不存在的 session；`snapshot.rs:552-565` 对 AX 节点的 backend ID/AX node ID 分配。
- **影响**：长时间动态变化的页面，旧节点映射可能持续堆积。
- **证据等级**：无页面内节点级回收逻辑已确认，增长速度未量测。
- **最小修法**：成功观察完成后，以“本次仍出现的键”对 session 内映射做代际清扫；保持当前帧的 locate/拒绝过期动作机制，不能直接回收可能仍被并行动作使用的 ID。
- **回归测试**：单 session 100k 次新增/删除 DOM、观察后 map 的容量上限、相同节点 id 稳定、已消失目标被拒绝。

### F07 | P2 | Refresh Token 集合无容量限制

- **代码证据**：`crates/rig-edge-browser/src/auth.rs:31-35,76-105`；每次 login 创建新的 refresh，只有刷新相同 token 时才移除旧 token。
- **影响**：持续重复登录会增加进程内存与活动 refresh 凭证数量。
- **证据等级**：源码已确认；项目设计本来就是单用户、Refresh 无时限且重启即吊销。
- **最小修法**：不改变已决的续期机制；增加显式撤销/活动会话上限或最近登录替代旧会话的可配置策略，先得到安全策略授权。
- **回归测试**：会话上限、并发 refresh、一张旧票只能刷新一次、重启全部失效。

### F08 | 已知安全欠账 | DOM 镜像仅遮蔽 `type=password`

- **证据**：`crates/rig-edge-browser/src/dom.rs:43-44,88-101,181-185`；项目文档 `docs/10-路线图与已知局限.md` 已明确列为后续安全决策项。
- **影响**：信用卡、OTP 等敏感字段不一定是 `type=password`。DOM 人工观看属于用户可见通道，而不是 AI 脱敏通道；扩展敏感判定可能影响既有使用与功能，需先确认需求及授权。
- **建议**：按具体 DOM 字段语义和安全边界设计掩码策略，并验证快照、增量、属性和输入旁路四路径一致。**不是新发现的遗漏，不得悄悄扩大安全规则。**

## 3. 性能优化机会（先验证基线）

### O01 | P1 性能方向：AI 局部观察仍做整页 CDP/AX/DOM 采集

- `crates/rig-edge-browser/src/ai.rs:472-494`: `observe_req_inner` 调用 `observe_full` 后才进行 `subtree`、`budget_tokens`、`layout` 投影。
- `crates/rig-edge-browser/src/snapshot.rs:235-338`: 按 session 获取 AX tree、DOM map、局部帧 AX、必要时二次 DOM map、批量几何。
- 项目自身 `docs/10-路线图与已知局限.md` 的 **G3/G8** 明确此为已知缺口；`since_frame` 现阶段主要降低回包，不保证减少采集成本。
- **建议**：先测静态页/局部文字修改/列表千行/跨源 iframe 的 `last_snapshot.total_ms, cdp_calls, cdp_ms, ax_dom_ms, quads_calls, nodes` 和 `latency.observe p95`。确认瓶颈后，用 CDP Nodes 变化日志构建**保守增量局部重算**，按 R1/R3 保持未变化不发送。滚动、CSS-only、子帧、导航与敏感属性变更必须允许全量回退，切勿把“无通知”误判“无变化”。

### O02 | P2：视频路径的全帧 RGB 内存复制

- `crates/rig-edge-browser/src/human.rs:336-359`: `compose_on_clean` 每个被接纳的内容帧都会 `base.clone()`，之后粘贴小块；有意使用 clean base 避免旧位置残影，不能粗暴改为“叠加在上一次合成帧”。
- `crates/rig-edge-browser/src/media.rs:536-541`：`crop_even` 尺寸已经是偶数时仍 `rgb.clone()`；媒体管线为 **10fps**，即使每次同尺寸仍复制整帧。
- 以 **1280×800 RGB24** 为例，每次全图复制约 2.93MiB（3,072,000 bytes）；HLS 10fps 单这一处理论上约 29.3MiB/s 内存搬运，不含实际编码耗时；这**不是实测带宽或 CPU 利用率**。
- **建议**：优先改 `crop_even` 在尺寸匹配时借用源图像（`Cow` 或临时引用），仅奇数边时拷贝；然后评估 content compose 的可复用缓冲池/双缓冲。压测旧位置残影、帧大小变化、不同观看端读同一缓冲的并发。

### O03 | P2：HLS 默认双路视频编码（可直接使用已有开关）

- `crates/rig-edge-browser/src/media.rs:320-342,383-409,489-498`：默认 HEVC/H.264 两个 rendition 对同一帧分别编码。
- 项目**已经支持** `RIG_HLS_SINGLE=h264` 或 `RIG_HLS_SINGLE=hevc`；`media.rs:75-138` 按播放列表请求启动并在 12s 无流量后释放，不是永久空转。
- **建议**：仅在部署客户端编解码能力明确时 A/B 测单 rendition，比较相同视频/分辨率下 CPU、功耗、首帧延迟和兼容性；未知浏览器环境继续双版本，切勿删除现有兼容兜底。RTC 是另一条独立的视频编码路径，并非 HLS 启用就总开。

### O04 | P2：截图脱敏与上下文校验链路可优化，但不得以牺牲安全换速

- `crates/rig-edge-browser/src/ai.rs:726-739` 截图仍执行 `observe_full`；`ai.rs:~683-723` 在 capture 前后验证敏感区域和页面上下文；`ai.rs:2045-2062` 对抓到的图片做解码/尺寸校验。
- `clip` **已经实现**，默认截图不覆盖存储帧的修正见 `plan/RIG-操作闭环优化-下一步测试方案-2026-11-24.md`；不要建议重复开发。
- **建议**：将采集、敏感节点扫描、屏幕 PNG 编解码、绘制掩码各阶段独立计时；在 0/10/100 个敏感节点场景做 p95 对照。优化几何查询合并或具有上下文代次校验的缓存，但掩码失效时必须拒绝或回退，不能直接复用旧脱敏信息。

### O05 | P3：批量动作需明确并发语义

- `crates/rig-edge-browser/src/ai.rs:847-895` 的批量操作是多次 `act_with_fresh`，各步独立获取动作锁，存在两个批次交错的理论可能。
- 项目当前定义的是**单用户受信工具**，文档并未承诺整批原子性。若将来确实需要批次间互斥，应由架构定版后实现“外层单次上锁、内部无锁单步”以免死锁；先补并发反例测试，不建议凭空改变批量失败语义。
- 项目 `plan/RIG-操作闭环优化-下一步测试方案-2026-11-24.md` 报告过在特定实验的批量工具使用能降低调用次数，属于该历史实验，不是本次性能实测。

## 4. 四专家视角归纳

| 视角 | 结论 |
|---|---|
| 架构 TL | 保留现有 Rust/Tokio/CDP 双通道与“操作/观察分离”契约。先修错误坐标和生命周期，再做局部观察；保留 fail-closed 行为和已批准安全边界。 |
| 技术 TE | F01-F07 有源码位置证据，实机触发比例和收益没有量测。所有性能优化都须在 release 模式和同一夹具下 A/B。 |
| 测试 QA | 补深层 OOPIF、pump 故障恢复、DOM iframe 持续替换、HLS 停机超时/磁盘故障、长时内存斜率的可复现回归；不要只测 happy path。 |
| 运维 SRE | `/health` 已有 p50/p95、CDP 和帧泵指标。建议补采集健康阈值、HLS 写包错误、重启后清理结果；告警基于症状（无新帧/播放列表停滞）而非仅进程存活。 |

## 5. 性能验证计划（只提建议，尚未执行）

| 场景 | 指标 | 对照和断言 |
|---|---|---|
| A 无观察者静态页 10min | CPU/RSS、`pump.full/content`、帧发送 | 需求为零时应无主动整页帧泵；检查空闲 CPU |
| B 仅 AI observe，静态/局部 DOM/大表 | observe p50/p95、CDP calls、AXDOM ms、回包 bytes/token | 分别测试 tree/blocks、subtree/budget、since_frame，区分采集代价和回包减少 |
| C 1 人观看 + 动态视频 | compose/encode p95、FPS、dropped、RSS | 旧实现对照 `RIG_PUMP_FULL_ONLY=1` 作诊断；不能仅优化平均值 |
| D HLS 兼容性 A/B | CPU、分片延迟、首帧、HEVC/H264 可播率 | 默认双路对照 `RIG_HLS_SINGLE=h264`；必须测真实浏览器 |
| E 长时 SPA iframe/DOM churn | RSS/JS heap、IdMap entries、watched.size | 多次增删后应回到稳态，且不存在已丢失元素的可点击坐标 |
| F 故障注入 | last_frame_age、media.last_error、重启次数、资源数 | CDP 重连、mux 写失败、线程超时、通道开关反复 100 次 |

**建议阶段顺序**：

1. 先固定 release 性能基线、回归样本和可观测指标，不先改大架构。
2. 修 F01、F02、F03、F05 的确定性行为并补自动化；隔离 F04 的目录/线程竞态。
3. 低风险微优化先试 O02 的无拷贝 HLS fast path，配置 A/B 试 O03。
4. 最后基于观察链路实测决定是否实施 G3/G8 的增量采样；严格保留安全和契约测试。

## 6. 本轮实际验证记录

**成功执行**：

```text
python -m unittest discover -s tests/eval_report -p 'test_*.py' -v
# Ran 6 tests ... OK
node --check crates/rig-edge-browser/src/viewer.js
# exit 0
node --check crates/rig-edge-browser/src/auth.js
# exit 0
node --check .temp/review/dom-injected.js
# exit 0（从 dom.rs Rust raw string 提取，不修改仓库源）
python -m compileall -q scripts eval tests
# exit 0
```

**逻辑模拟（非项目集成测试）**：以 `snapshot.rs:343-381` 的原地更新顺序模拟五层逆序 OOPIF，四轮后第 5 层 `placed=false`。

**未执行 / 环境限制**：当前容器没有 `cargo`、`rustc`，所以 Rust 格式、clippy、构建、单元/集成测试都未运行。Playwright 浏览器探针先遇到缺少其缓存 Chromium；改用 `/usr/bin/chromium` 并尝试 file:// 和本地 HTTP 后，均被沙箱策略 `net::ERR_BLOCKED_BY_ADMINISTRATOR` 拦截，未形成有效 Chromium E2E 证据。未提供线上负载、trace、CPU/RSS 时序，因此没有性能加速百分比或根因实测结论。

在有 Rust/Chromium 的工作站应执行：

```bash
source scripts/local-env.sh
cargo fmt --all --check
cargo clippy --all-targets -- -D warnings
cargo test --all
cargo build --release -p rig-edge-browser
python eval/verify_fixtures.py
```

> 此列表为**待执行命令**，不能解释为本次 PASS。计划文档提到的历史 39 项 Chromium 集成测试和 102 项单测也是历史记录，并非本次验证结果。

## 7. 待决与边界

- 是否允许改 AI 元素 ID 的生命周期和未定位 iframe 的输出语义？这涉及项目已批准的动作/观察契约，应先认可最小修正方案。
- 是否允许限制同一主令牌下的 refresh 活动数量？这是安全策略改动，不应擅自执行。
- 实际部署是否存在 HLS 双编码的必要性？单 H.264 可节省一路编码，但会减少原 HEVC 路径。
- 是否能提供 release 模式下两组 `/health` 指标和 1–2 段真实慢页面 trace？这将决定 G3/G8 是否有足够的投资回报。

**本次交付**：静态源码审查、证据分级、低风险性能优化与回归方案；没有修改原始项目或声称完成独立验收。
