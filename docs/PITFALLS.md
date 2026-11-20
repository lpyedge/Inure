# PITFALLS.md — 浏览器边缘踩坑手册

> LIU 2026-11-20 定版要求：所有发现的问题与解决方案汇总成册，确保以后随时可查、不再踩坑。
> 每条格式：**症状 / 根因 / 解法 / 验证**。提交索引见 `docs/EDGE_BROWSER.md` §13；本文只记教训，不记流水账。
> 维护规则：新坑先落本文，再考虑是否同步进 EDGE_BROWSER §11（已知限制）。

## 一、帧与合成

### 1. 观看端 RTC 恒黑屏
- 症状：RTC 连通但画面全黑，WS 帧正常。
- 根因：观看端 `ontrack` 把视频流整体赋给 video 元素，随后到达的音频流又整体覆盖一次，只剩音频。
- 解法：按 track kind 分别挂载（video → srcObject 的视频轨、audio 合并进同一 stream 的音频轨）。
- 验证：真浏览器探针看 videoWidth/像素（`91a94f3`）。教训：IT 套件当时没有 RTC 画面断言，黑屏正是漏网原因。

### 2. screencast JPEG 满屏绿色
- 症状：人通道画面解码后大面积绿色伪影。
- 根因：image/zune-jpeg 解 Chromium screencast 的真实帧（特定子采样/分量布局）系统性出错，合成测试图却正常。
- 解法：JPEG 解码一律走 ffmpeg；用一张真实 screencast 帧作 fixture 加回归测试。
- 验证：fixture 测试 + 真实帧目检（`052ff54`）。

### 3. 合成帧高 ≠ 页面视口高
- 症状：按视口高换算的点击坐标整体偏移。
- 根因：合成帧 = chrome 条带（标签条+导航条+查找条）+ 页面图原生拼接，帧高比视口高多一个条带。
- 解法：坐标换算一律用真实帧尺寸：页面点帧内 y = (帧高 − 视口高) + 页面 y；边缘侧 page_coords 同理减条带高。
- 验证：探针以远端 activeElement/计数器为点击成败判据，不看坐标自洽。

### 4. health 的 frame_size 会停在奇怪的小尺寸（如 324×124）
- 症状：health 报 frame_size 324×124，像是视口被改小。
- 根因：双因——(a) last_size 只是「最后一帧」的尺寸，静态页在视口 resize 途中收到一帧中间尺寸后不再有新帧纠正；(b) 更常见的是**僵尸边缘实例**：重启命令回报成功但新实例因端口被旧实例占用而静默退出，health 一直由旧实例应答。
- 解法：判活/取 PID 以 `ss -ltnp` 的监听 PID 与 `ps lstart` 为准，绝不信启动命令的回报；重启后必须核对新 PID 与构建时间。
- 验证：按 ss 核实 PID 后 health 回到 1280×800。

## 二、输入与剪贴板

### 5. 拖选不跟随
- 症状：视频模式里鼠标拖动选不中文字。
- 根因：CDP 的 mouseMoved 在拖动期间必须同时带 `button` 与 `buttons` 掩码，缺一个浏览器不认为是拖动（Playwright 协议日志实证）。
- 解法：拖动中的 mouseMoved 一律带按下按钮与掩码（`6fd1f7c`）。

### 6. Ctrl+V / Ctrl+C / Ctrl+X 在远端不生效
- 症状：快捷键转发过去，页面没有粘贴/复制动作。
- 根因：观看端 keydown 处理里 preventDefault 掉了快捷键的浏览器默认动作，远端收到的按键序列不完整。
- 解法：这三组和弦的 keydown 不压默认动作（`ec86881`）。

### 7. 外部复制 → 内部粘贴被覆盖（架构病）
- 症状：在外部复制新内容后到镜像里粘贴，贴出的还是旧内容。
- 根因：旧实现有「远端剪贴板暂存 + 多个触发点（mousedown/keydown capture）异步补写本地剪贴板」的机器，补写与用户的新复制竞态，把新内容覆盖回旧值。
- 解法：**显式模型**（借鉴 Go 参考实现 personal-agent）：复制 = 一次请求/回应（观看端发 clipboard-get → 边缘读远端当前选区回传 → 观看端写本地或弹手动复制框）；粘贴靠真实 paste 事件。整删暂存/补写机器（2026-11-20 本批）。
- 验证：probe-batch4 B2/C/F 全过；HTTPS 下走标准 Clipboard API 直写（dbg18）。

### 8. 非安全上下文里 Clipboard API 不可用
- 症状：局域网 HTTP 原点下 navigator.clipboard 不存在/恒拒绝。
- 根因：Async Clipboard API 仅安全上下文可用；但 copy/cut 事件内的 clipboardData.setData 与 paste 事件读 clipboardData 不受此限（这条曾支撑过渡方案）。
- 解法：LIU 定版——边缘自签名证书上 HTTPS（见 §五 15），剪贴板回归标准路径，不再为非安全上下文堆变通。过渡方案（边缘轮询同步+事件内兑现）已全部拆除。
- 验证：dbg18：isSecureContext=true、Ctrl+C 直写本地剪贴板成功、无手动框弹出。

### 9. DOM 模式复制出来的不是选中的文字
- 症状：在镜像里选中文字复制，得到的是别的内容。
- 根因：镜像 Ctrl+C 曾转发远端执行，远端选区与镜像选区是两个选区。
- 解法：镜像复制以**镜像文档自身选区**为准（输入框选区或 getSelection），不转发远端；边缘回读也按镜像选区语义实现。
- 验证：probe-batch4 F 过。

### 10. 光标恒为 I 型
- 症状：视频模式里鼠标在任何内容上都是选字光标。
- 根因：输入覆盖层是 textarea，默认光标即 I 型，与远端内容无关。
- 解法：`#ime { cursor: default }`（`96d7e9c`）。

## 三、右键菜单

### 11. 原生上下文菜单抓不到
- 症状：视频模式右键无菜单；DOM 模式右键不生效。
- 根因：Chrome 原生菜单是**浏览器 UI 层**，不在页面表面里——screencast、captureScreenshot、DOM 序列化任何手段都拿不到（探针实证）。
- 解法：自绘菜单是唯一路。v1 曾做页内注入菜单（会污染页面、页面自定义菜单需让行）；**现行 v2 = 观看端绘制**：菜单项为桥接动作（复制选区/粘贴/全选/后退/刷新），右键本身照常转发页面，页面自定义菜单不受影响（2026-11-20 本批，借鉴 Go 参考实现）。
- 验证：probe-batch4 B1/G1 过。

### 12. 「复制图片」按坐标取图不可靠
- 症状：菜单里复制图片拿到空数据或错图。
- 根因：镜像与远端的行内流布局会因字体度量不同而漂移（实测同图中心差 ~120px），按镜像坐标回远端 elementFromPoint 命中错误元素。
- 解法：按**节点 id**取图：序列化器注册表 `__rigDom.imgData(i)` 在远端按 id 定位 IMG 本体读像素；视频模式无此问题（帧坐标即远端坐标）才用按坐标取图。
- 验证：probe-batch4 G2 过。推论：一切镜像→远端的定位优先用 id，坐标只作兜底。

## 四、DOM 镜像

### 13. DOM 资源 401（YouTube 播放器 CSS 被向边缘源请求）
- 症状：镜像里 `GET http://<边缘源>/s/player/.../www-player.css` 返回 401，样式/图片大面积丢失。
- 根因：镜像文档的原点是边缘源，序列化保留的相对/根相对 URL 全部按边缘源解析；带 token 的接口又 401。
- 解法：序列化时**全量绝对化**：元素 href/src/poster/action/data/background 属性、srcset、STYLE 文本与样式表内的 url()（以样式表自身 href 为基准）、@import 内联内容（以 import href 为基准）、属性差分（'at' op）同口径；观看端建树前先在未连接的新树 head 放 `<base href=页面URL>` 再 replaceChild（顺序反了 base 不生效）。
- 验证：probe-batch4 D1–D6 全过（根相对/相对 CSS、@import、CSS 背景、图片 naturalWidth、无一资源落到边缘源）。
- 已知边界：无法内联的跨源 @import 兜底行仍保留原样相对引用——其 cssText 的正确基准是父表 URL，serRules 当前无此信息；真实页面罕见，遇到再补。

### 14. 序列化器三条定案（与 §13 配套，详见 EDGE_BROWSER §9.2）
- 样式表自身 media 必须保留包裹，否则 print 表在镜像里无条件生效（如 OSM 把侧栏全藏）。
- `@media (scripting: …)` 纯门控必须改写：镜像禁脚本会把 scripting 报为 none，误触发页面的无-JS 降级样式。
- 同源 @import 递归内联（depth<4），跨源保留原行。

### 15. 跨源 iframe：同源策略的边界在哪
- 症状：Google 验证码等跨源 iframe 在镜像里只有空占位。
- 根因：同源策略限的是**页面脚本读跨源 iframe 内容**；但 CDP 的坐标型输入由浏览器合成层路由、captureScreenshot 的 clip+fromSurface 是浏览器级合成面，两者都**不受**同源策略限。
- 解法：跨源 iframe 与 video 走**区域子流**：序列化器标记节点（x 标志 + rect + display），边缘按 rect 周期截图（跨源 iframe ~1.2s/rect 变化、video 播放 ~300ms、暂停按签名）广播 dom-region，观看端贴回占位。
- 验证：probe-batch4 E1/E2（点击穿透、计数递增）/E3（视频播放+区域帧更新）全过。
- 注意：区域截图是像素补位，验证码能看能点，但 iframe 内部滚动/嵌套交互不镜像（§11 已知限制）。

### 16. 镜像占位块把布局挤变形
- 症状：镜像里跨源 iframe/视频的位置与远端对不上，点击穿透落空。
- 根因：占位用了块级 div，把远端 iframe 的行内流挤换行；且远端 iframe/video 的 computed display 是 `inline`（替换元素认宽高），普通 div 的 inline **不认宽高**，照抄 display 会塌成 0×0。
- 解法：占位保持原 display 性质，但 `inline` 一律映射 `inline-block`（认宽高且保持行内流）。
- 验证：dbg10 实测占位 rect 与远端一致后 E2b/E3 转绿。

### 17. 每个手势被转发两次（视频点一次变播放+暂停）
- 症状：DOM 模式点击像双击、菜单开两次、输入工作量翻倍。
- 根因：镜像文档对象跨快照存续，观看端每份快照都给它重挂一遍输入监听。
- 解法：attachMirrorInput 加幂等守卫 `doc.__rigAttached`，同一文档只挂一次。
- 验证：E3 单击播放稳定（本批）。

### 18. DOM 滚动停半途 / 镜像收不到指针
- 症状：镜像滚不到底；DOM 模式下点镜像无反应。
- 根因：双因——(a) 全屏 #ime 覆盖层在 DOM 模式仍截走镜像的指针事件，且其坐标换算对着隐藏画布算出空值被丢弃；(b) 序列化器 scroll 节流丢掉停稳的最后一发位置。
- 解法：(a) DOM 模式给覆盖层 pointer-events:none；(b) scroll 改尾随节流（trailing push 保最后一发）（`96d7e9c`）。
- 教训：IT/探针以文档内直发事件绕过命中测试，曾长期掩盖 (a)——涉及真实指针路径的改动必须用真实坐标点击验证。

### 19. DOM 增量静默死亡（本轮最隐蔽的一坑）
- 症状：DOM 模式快照正常、点击能转发，但页面变化永远不再同步（延迟探针 -1ms = 永不到达）；新开会话又正常，老会话必死。
- 根因：三处叠加——
  1. **viewers 计数泄漏**：WS 客户端断线（关标签页/探针退出）不发 dom-stop，边缘 viewers 计数只增不减；后来者的 dom-start 被当成「后来者」只补快照，不再调 setup/start。
  2. **导航后新序列化器无人启动**：页面刷新/导航后序列化器是新实例（观察者未启动），pump 收到 'installed' 只补快照、不补 start()；视图未变时 tick 也不补。
  3. **pollAv 误杀**：观看端 pollAv 的旧逻辑「AV 到达就退出 DOM」，仅靠 domAvOffPending 的 6 秒窗保护；窗一过 pollAv 就 domStop，连带边缘停掉观察者。
- 解法：每次 dom-start 一律 setup_session+快照（setup 对已装会话只补一发 start() eval）；pump 的 'installed'/'resync' 先 setup 再快照；WS 断线时若本连接占着 DOM 名额补一次 viewer_stop；viewer_stop 计数钳制下限 0（try_update）；pollAv 在 domOn 时完全不抢占。
- 验证：dbg17（刷新后连续 8 次突变增量全达、16s 稳定）+ probe-batch4 连续两轮 22/22（H 延迟 55ms）。

### 20. DOM 模式「卡顿」先分清哪一层
- 症状：用户报 DOM 操作卡。
- 分解：增量节流 100ms 时主观卡 → 降 30ms 后隔离测 55–110ms；但满载（视频区域子流+debug 构建）下偶见 ~1s。后者多为单泵串行（region_sweep 的截图 await 挡住 binding 处理）与 debug 构建开销，release 复测为准，不要把构建差异当产品结论。

## 五、站点兼容与传输

### 21. YouTube 停在预览图不播放
- 症状：YouTube 打开只有预览图，播放器不初始化（视频/ DOM 两模式同）。
- 根因：边缘自拉 Chromium 的 UA 带 `HeadlessChrome`，触发 YouTube 机器人墙/播放器拒绝；webdriver 本身为 false、远端媒体栈健康（本地 WebM/MP4 样本可正常播放，canPlayType "probably"、进度推进）。
- 解法：UA 规范化——连接建立、挂载任何页面会话前，用 Browser.getVersion 取本体 UA 并把 HeadlessChrome 替换为 Chrome，对每个 page 会话设 Emulation.setUserAgentOverride；`RIG_EDGE_UA` 环境变量可整串覆盖。
- 验证：dbg19：墙文本消失、readyState 4、currentTime 持续推进（6.2s→11.3s）。
- 备注：若日后个别站点仍拒播，先分清 UA/指纹与机房 IP 信誉墙——后者不硬攻，如实归类。

### 22. 自签名证书与 HTTPS（LIU 定版路径）
- 背景裁定（LIU 2026-11-20）：「复制粘贴的问题回头用自签名证书来测试解决，如果不行就信任根证书，不要反复折腾了。」
- 实现：`--tls-cert/--tls-key` 成对使用；文件缺失时 rcgen 自动生成自签名（SAN = localhost、127.0.0.1、监听 host），私钥 0600 落盘复用；观看端 WS 按 location.protocol 自动 ws/wss、HLS 走相对路径，无需改观看端。
- 验证：证书自动生成 + HTTPS health + isSecureContext=true + 剪贴板标准 API 直写（dbg18）。

### 23. str0m / DataChannel 三条硬事实
- str0m 会话的 UDP 必须绑在**通告的候选地址**上，否则 STUN 被静默丢弃、ICE 永不连通（Phase 1 最大的一坑）。
- 浏览器 SCTP 单条消息上限约 256KB（Chrome）：DOM 快照走 DC 必须应用层分片（16KB，`a0fd18b`）。
- str0m `Channel::write` 窗满返回 Ok(false) 且**不排队**：发送侧要自备待发队列、排空重试（`23ef07e`）。

### 24. DOM over DC 快照永不到达（历史）
- 症状：DC 开后 DOM 快照不到，关后回落 WS 正常。
- 根因：即 §23 的分片+窗满两条叠加，再加 pollAv 竞态误杀（已由 §19-3 根治取代 domAvOffPending 方案）。
- 验证：`180540c` 后 DC/WS 双路均达。

### 25. 指纹电池：无头/CDP 形态的可检测面（2026-11-20，LIU 定向）
- 背景：LIU 定向——这浏览器是给人和 AI 用的，不能保留 headless 或 CDP 开发模式的效果，站点检测会让用户无法操作。探针 dbg24 对正式服务浏览器跑全套检测项，先量基线再逐项修。
- 基线本就干净（不要误伤）：`navigator.webdriver=false`（边缘自拉 Chromium 不带自动化标志；Playwright 启动的浏览器 webdriver=true，两者不可混为一谈）、plugins 5 项标准 PDF（全量构建才有）、`window.chrome`/`loadTimes` 在、CDP console 探针（对象 id getter + console.log 序列化）不触发（边缘不启用 Console 域）。
- 坑一：**只改 UA 串不带元数据 → UA-CH 高熵值整体清空**。`Emulation.setUserAgentOverride` 只给 userAgent 时，`navigator.userAgentData.getHighEntropyValues` 返回 brands=[]、platform=""、uaFullVersion=""——真 Chrome 恒有值，空元数据是更显眼的异常。解法：同调用带完整 userAgentMetadata（brands/fullVersionList/platform/architecture/bitness/formFactors）。
- 坑二：**完整版本号要取 product，不要取 UA 串**。UA 串里的 Chrome/ 版本是约简版（159.0.0.0），而浏览器按真实版本回填 uaFullVersion（159.0.8154.6）；fullVersionList 若用约简版，两者对不上又成一条不一致。`Browser.getVersion` 的 product 段才是真实完整版本。
- 坑三：**`--disable-gpu` 让 WebGL 整体不可用**（getContext('webgl') 返回 null）。真 Chrome 恒有 WebGL（最差软件渲染），无 WebGL 既是硬信号也弄坏地图/播放器/验证码。解法：去掉该标志 + `--enable-unsafe-swiftshader`（新版 Chrome 软件 WebGL 回落需显式放行）；无 GPU 主机自动回落 ANGLE/SwiftShader。renderer 字串含 SwiftShader 是残留软信号（VM 与远程桌面同款），不在此层处理。
- 坑四：**虚拟屏 800×600 比窗口（1280×800）还小**。新版 headless 的 screen 尺寸不随 --window-size 走，screen < window 在真实桌面物理不可能。解法：`Emulation.setDeviceMetricsOverride` 连带 screenWidth/screenHeight = 视口（最大化形态）。
- 坑五：**`--lang` 在无头模式下不驱动 navigator.languages / Accept-Language**（实测传了 --lang=ja-JP 仍是 en-US）。解法：经 setUserAgentOverride 的 acceptLanguage 下发；且**值不带 q 值**——CDP 会把 acceptLanguage 原样拆进 navigator.languages，`ja;q=0.9` 这种条目真浏览器永不出现，请求头缺 q 值只是小瑕疵，JS 可见面优先。
- 坑六（边界）：时区默认 UTC 与人格不符 → `Emulation.setTimezoneOverride`（默认 Asia/Tokyo，RIG_EDGE_TZ 可改）；以上人格项**仅自拉浏览器套用**，挂靠模式挂到真实浏览器时整段跳过（碰用户自己浏览器的身份是越界）。DOM 模式用过的页面留 `__rigDom`/`__rigSend` 全局，是已知残留（新页面干净）；出口 IP 地理属部署层，不在此层。
- 验证：dbg24 前后对照——UA-CH 全字段有值且 fullVersionList 与 uaFullVersion 一致、languages=[ja-JP, ja, en]、tz=Asia/Tokyo、screen=1280×800、WebGL 恢复、webdriver 仍 false。
- 待议（LIU 已提）：改 Chromium 源码去掉可识别面后自编译——构建需大机器（ovhca 16G/本机均不够格），且补丁面（WebGL renderer 伪装、CDP 痕迹）要逐项评估收益；先以本条的配置层为准，站点实测再有具体拦截案例时按案例升级。

### 26. 地址栏右键无效、粘贴进不了 URL（2026-11-20，LIU 实测反馈）
- 症状：视频模式右键地址栏弹的是页面通用菜单且「粘贴」无效；DOM 模式条带右键完全无反应；左键点进地址栏打字/粘贴却都正常（探针分层：打字通、合成 paste 事件通、边缘 text_input 按编辑态路由通）。
- 根因一（边缘）：human.rs 鼠标按下处理里中/右键**从不与工具栏交互**（只直达页面、落在条带上忽略）——右键点地址栏永远不进编辑态，菜单「粘贴」发出的文本按「页面焦点」路由，落进页面而不是地址栏。
- 根因二（观看端）：DOM 模式条带 canvas 的 contextmenu 只 `preventDefault()`，菜单根本不开。
- 解法：右/中键落在地址栏命中区（Hit::Address）时例外处理——先 chrome_press(Address) 进编辑态再吞掉这次按下；条带右键改开同一套观看端菜单（坐标经 stripPos 换算回帧坐标）；菜单命中地址栏区域时多一项「粘贴并转到」（粘贴 + Enter）。地址栏区域判定只决定菜单内容（几何与 chrome.rs Geo 同口径在观看端复算），粘贴对错靠边缘编辑态路由，不靠这个判定。
- 验证：dbg27——视频/DOM 两模式右键菜单均含「粘贴并转到」，点击后远端分别导航到剪贴板里的 URL。

### 27. 视频模式「太卡顿」：帧率被三层钳住（2026-11-20，LIU 实测反馈）
- 先分层实测（RTC getStats 解码帧率）：静止页 0.3fps 是截屏无损伤帧的正常表现，不是卡；运动源才作数。
- 钳一（常量）：rtc.rs `RTC_FPS=15` 把 feeder 节拍钉死在 15fps——改 30；BWE 上限 4M→8M（直连/局域网是当前主形态，1300×990@30 文本画面 4M 不够；下限与起始 2.5M 不动，WAN 时 BWE 自己会压）。
- 钳二（帧泵吞吐）：分段计时（release，1280×800 真实帧 fixture；`bench_frame_stages`）——解码 ffmpeg 4.6ms、合成稳态 0.2ms（条带缓存命中 + 无菜单/指针时整幅字节拼接快路径；冷启动那一轮的 16ms 是一次性字体加载+条带首绘，别误判）、JPEG 编码 image crate 纯 Rust **34ms**（最大单项）→ 改 ffmpeg mjpeg（qscale 5）**9ms**、109KB/帧。整泵从 ~59ms/帧（≈15fps 上限）降到 ~14ms/帧。另外帧泵跟不上时要**排水到最新帧**再处理（排队旧帧只有延迟没有信息量；被跳过的截屏帧逐个 ack），JPEG 只在确有 WS/条带订阅者时编（每条 WS 连接都订阅广播、RTC 连接只是收下丢弃——receiver_count 恒 >0，不能拿它当需求信号；订阅在 main.rs 连接层，本条只落了「按需编码」的结构与排水，需求计数未做，mjpeg 提速后暂不需要）。
- 钳三（测试素材陷阱）：测帧率的 sample.webm 本身是 **15fps**（testsrc 默认率）——管线修好后复测仍显示 15.0，一度误判修复无效。测帧率先 `ffprobe` 素材帧率；动画页用 rAF 驱动（60Hz）更可靠。修复后实测：rAF 动画页 30.4fps、60fps 视频 30.3fps（2.5Mbps、0 丟幀），均顶到 RTC_FPS 上限。
- 对照参考实现：Go 版按截屏原生帧率转发 JPEG（零解码透传 + ack 背压），其流畅来自没有帧率钳与编码成本；RIG 因 chrome 合成结构性多一层，只能靠把每层成本压下去追平——本条即全过程。

### 28. 镜像怪异模式布局漂移 + 焦点恢复（2026-11-20，LIU 实测反馈「DOM 粘贴进测不出来」）
- 症状：DOM 模式下点小输入框，远端焦点落空，粘贴/输入随之无效；大文本框却偶尔正常——「时灵时不灵」。
- 定因（dbg30 落点记录）：镜像文档出身 about:blank，恒为**怪异模式（BackCompat）**；远端标准模式（CSS1Compat）页面的流式布局与之系统性漂移——实测 dyntest 页输入框在镜像里比远端高 13.5px，转发点击落在其上方 BODY 上，远端 activeElement 始终 BODY，粘贴事件在镜像里明明带着正确文本触发（dbg28 计数实证），边缘 insertText 却无处着落。
- 解法：序列化器快照带 `mode: document.compatMode`；观看端发现与镜像当前模式不一致时以 srcdoc 重载镜像文档对齐模式（`<!doctype html>` 有无决定标准/怪异），快照暂存、iframe load 后再应用。重载后同一探针点击精确命中（落点与远端 rect 中心重合、activeElement=dyn）。
- 伴生修复（焦点恢复）：快照换 documentElement、差分 rm/add（框架重渲染换节点）都会把镜像焦点打回 body——此后 Ctrl+V 的默认粘贴没有可编辑目标。观看端在 focusin 时记住焦点控件的 rig id + HTML id 签名 + 输入选区，快照/差分应用后若焦点已丢，按 rig id（或同 HTML id 的替身节点，专治节点被整体替换、rig id 新分配的情况）恢复焦点与选区；只动观看端本地焦点，不给远端发任何东西。动态重建页（focus 后 400ms 整体换节点）实测：真实 Ctrl+V 粘贴成功落远端。
- 教训：镜像保真不只是树与样式——**文档渲染模式**也是布局输入；凡「坐标恒定偏一截、小目标必偏、大目标偶尔蒙对」的症状，先查 compatMode 与视口尺寸这类全局布局参数，不要逐个元素对 rect。

### 29. 自动模式判据：纯面积比区分不了「嵌小视频的表单页」与「视频页」

- 症状：F-08 自动模式初版以视频面积比单阈值判页型：5% 时表单测试页（嵌 320×180 暂停视频）被判视频页；提到 20% 后视频测试页（480×270 播放中，面积比仅 14–16%）又被判 DOM 页——两个方向都错。
- 定因：两类页面的视频面积比实测只有个位数百分点之差（≈9% vs ≈14–16%，且比值随快照视口口径漂移），任何单阈值都落在重叠区。真正的区分特征是**交互密度**：表单页可交互元素多、视频页几乎为零。
- 解法：组合判据——视频面积 >10% **且**可交互元素 <3 才判视频页；跨源/视频区域 >30% 或 canvas >20% 独立判视频。dbg35 双场景实测：表单页自动进 DOM、视频页保持视频。裁决明细（各比值 + wantDom）暴露于观看端 `window.__rigAuto`，判据再校准时先看数据不猜。
- 教训：拿真实页面分布校准判据，别拿想象中的典型值定阈值；两个类别的特征分布重叠时，换特征维度（密度/计数）而不是挪阈值。

### 30. last_frame 靠无关机制的副产物喂养（2026-11-20，第五期实踩）

- 症状：第五期删帧合成后，静态页面 + 新 WS 连接收不到任何二进制帧，IT 两项失败（end_to_end 与 chrome_ui 的收帧断言）。帧泵本身完好：动态页面帧流正常。
- 定因：新连接靠 `last_frame` 回放立刻有画面，而旧实现里 `last_frame` 只在「有 JPEG 需求时编码」才更新——静态页在无连接时需求恒 0。旧测试能过，是因为同步循环在 chrome 状态变化时**无条件重合成并广播**（启动瞬态的标题/加载变化总会触发几次），顺手把 last_frame 喂上了。回放能力实际寄生在合成机制的副产物上，删合成即断粮。
- 解法：帧泵透传原始 JPEG 后广播零成本——每帧无条件记 last_frame 并广播，需求记账对帧泵退役。
- 教训：删一个机制前，先 grep 谁在消费它的**副产物**（缓存、计数、最近值），不只是它的主输出；被副产物喂养的能力要在新实现里显式保住。

### 31. 起始页后退被 about:blank 顶亮（2026-11-20，第五期探针发现）

- 症状：边缘启动直达起始页，后退按钮却是可用态，点下去回到空白页——与真 Chrome 不符（起始页历史只有一条、后退禁用）。
- 定因：自拉 Chromium 的 argv 末参是 about:blank，边缘 attach 后再 navigate 到起始 URL，导航历史变成 [about:blank, 起始页] 两条，`nav_state` 按 currentIndex>0 判 canBack=true。
- 解法：起始 URL 直接作 argv 末参成为初始标签页（历史一条）；挂靠模式没有 argv 可用，仍 navigate（那是别人已开的浏览器，历史语义归用户）。
- 教训：凡「先开空白再导航」的启动序都会在历史里留幻影条目；浏览器保真问题先查导航历史形状，别先怀疑状态计算。

### 32. 子资源带不上页面 URL 的 ?token=：观看页拆分后带令牌访问只剩空壳（2026-11-20 上线实踩，LIU 实测）

- 症状：LIU 以 `https://…:39871/?token=…` 打开观看页：header 壳渲染出来，但标签条全空、导航按钮无图标、地址栏不回填、舞台全黑——JS 一行没跑。
- 定因：第五期把观看页拆成 `/` + `/viewer.js`，而 `<script src>` 子资源请求**不会**继承页面 URL 里的 `?token=`，被 auth_guard 401。页面本体靠 URL 令牌过的、脚本过不了。其余子请求（WS、fetch、HLS）都在 JS 里显式拼了令牌，唯独脚本标签拼不了。上线前探针与 IT 的观看页都没配令牌（无令牌时鉴权整体豁免），这条路径从未被测到。
- 解法：`/viewer.js` 在 auth_guard 里与 /health、/ready 同列豁免——静态脚本无秘密、内容对所有人相同；页面本体/WS/API 照旧查令牌。IT 的 edge_token_auth 补断言（`/` 无令牌 401、`/viewer.js` 无令牌 200）。
- 教训：**给页面加/拆任何子资源（script/css/img/worker）都要按「子资源拿不到页面凭证」检查鉴权**；带令牌的部署形态必须有一条真浏览器端到端探针（dbg38：?token= 打开 → 零 4xx、标签/状态/帧齐全），不能只测无令牌回环。

### 33. DOM 模式粘贴「无效」：远端已落字、镜像不显示（2026-11-20，LIU 实测反馈）

- 症状：DOM 模式下从外部复制文本、Ctrl+V 粘进镜像输入框——什么都不出现；内→外复制正常、视频模式双向正常。
- 定因：三段拼合。①镜像 paste 处理器 `preventDefault` 压掉原生粘贴、只把文本 send 给边缘（边缘 `Input.insertText` 落进远端真页，实测远端值正确）；②镜像的值回环（序列化器 val 差分）在**聚焦元素上按设计跳过**（防打字回退：本地回显先行、回环值滞后会把已打出的字冲回去）；③于是粘贴既无本地回显、又无回环显示——用户视角就是「粘贴无效」。打字没事是因为 keydown 不压默认动作、本地回显天然在。
- 解法：文本粘贴**不压默认动作**（与打字同口径：镜像原生粘贴作本地回显，send 照旧负责远端落字）；图片粘贴维持 preventDefault + paste-image 通道（无本地回显可言）。
- 教训：镜像里一切「本地回显 + 远端落字」的双轨输入（打字/粘贴/IME）都要成对检查两轨——压掉本地轨又指望回环轨显示时，先确认回环对聚焦元素是否豁免。探针 dbg39（镜像装 paste 监听 + observe 读远端值）可分段归因。

### 34. 镜像图标全空白：SVG 元素被当 HTML 元素重建（2026-11-20，LIU 实测 YouTube）

- 症状：DOM 镜像里 YouTube 版式大体成立，但所有图标按钮是空白圆圈/药丸（点赞、分享、菜单、站标全无图）。
- 定因：镜像构建器对所有节点一律 `doc.createElement(tag)`——svg/path/use 落进 HTML 命名空间，浏览器不按 SVG 渲染。与之无关的弯路：先疑「YouTube 组件在 shadow DOM 里、序列化器进不去」——实测现代 YouTube 组件是 **light DOM** 渲染（ytd-* 元素子节点齐全、无 shadowRoot），CDP pierce 数到的 41 个 shadow root 全是 INPUT/VIDEO/IMG/use 的 **UA 内建**影子树。序列化器（ser）加 `ns` 标记（namespaceURI 为 SVG 者），构建器改 `createElementNS` 且标签名不 lowercase（foreignObject 等大小写敏感）后图标全数恢复。
- 教训：归因先数实据（哪些标签挂影子根、节点在不在树里），别拿「众所周知用了 X 技术」当定因；镜像类系统对命名空间（SVG/MathML）与大小写敏感标签要专项对待。

### 35. 强杀 Chromium 丢登录态：存储落盘是异步的（2026-11-20，持久化 IT 首跑实踩）

- 症状：给边缘加了 --profile-dir 持久化，IT 里写入 Cookie/LocalStorage/IndexedDB、2 秒后整组 SIGKILL、换同目录重启——IndexedDB 在，Cookie 与 LocalStorage 全丢（新版 Chromium 的 LocalStorage 已是 SQLite 后端、与 Cookie 库同属延迟提交）。
- 定因：这几类存储都是异步批量落盘：写入先在内存/事务队列里，数秒内被 SIGKILL 就随进程蒸发；IndexedDB 因事务结束即提交而幸免。旧 shutdown() 只 `child.kill()`、且 SIGTERM 根本没接（只有 ctrl_c 接了 HTTP 的 graceful handle）——生产里用 kill 停边缘同样会丢最近的登录写入。
- 解法：边缘在 SIGINT/SIGTERM（HTTP 与 stdio 两模式）走优雅退出，`Browser::shutdown()` 先发 CDP `Browser.close` 请 Chromium 落盘关闭、超时才硬杀。部署停机用 TERM 不用 KILL。IT 的持久化用例按此口径：TERM 停机 + 落盘实据断言（Cookies 文件含 cookie 名、LocalStorage 文件含值）+ 重启恢复三值。
- 教训：**持久化不只是给对目录，还包括停机路径把数据送进目录**；凡「写后即杀验恢复」的测试失败，先分清是没写对地方、还是没等到落盘、还是停机方式把未落盘的写丢了——三者修法完全不同。

### 36. CDP mousePressed 不带 clickCount：连普通 click 都不产生（2026-11-20）

- 症状：给人通道加双击透传时把 page_mouse 的硬编码 clickCount=Some(1) 改成「消息没带 count 就不给」，三个原本全绿的 IT 用例当场全红（DOM 点击加不出节点、接管时序错位、新双击用例不达）——所有不带 count 的发送方（旧观看端、测试原始消息）点击全灭。
- 定因：dbg49 四变体实测（视频/DOM 两路 × 带/不带 count）：不带 count 时页面 mousedown 根本不达（c=0）；带 count=1,2 时 c=2 且 dblclick 正常。本机 Chromium 的 Input.dispatchMouseEvent 对缺省 clickCount 的处理就是不合成 click——Puppeteer/Playwright 永远显式带 clickCount，所以这坑在常规工具链里遇不到。旧硬编码 Some(1) 一直在默默兜底全部发送方，抽掉即塌。
- 解法：count 缺省在边缘侧补 1（两条输入路径同口径），真实 count（2/3）照常透传。
- 教训：**「可选参数不给」与「给缺省值」在 CDP 里不是一回事**；改任何注入参数的缺省行为，先用变体探针把「不给/给 1/给 N」三档在目标浏览器上实测一遍。

### 37. 浏览器内部页的 DOM 快照是空壳：shadow DOM 不在序列化树里（2026-11-20）

- 症状：设置/下载/扩展等 chrome:// 页在 DOM 模式下「无法显示」——不是报错，是镜像出来一片残页。
- 定因：探针实测序列化器在 WebUI 页上能注入、能产快照（设置页快照树 253KB），但 WebUI 的实际界面全在 shadow DOM 里，light DOM 只有自定义元素空壳——镜像忠实地复制了「壳」。视频模式（screencast）对 WebUI 正常出帧。
- 解法：DOM 通道在 snapshot() 一处按 URL scheme 把关（chrome://、chrome-extension://、about:、devtools://、view-source: 等）：内部页不产快照、直接广播 dom-state 错误（注明内部页），观看端既有 domFallback 切视频模式并提示。viewer_start/resync/感知泵三条来路都经 snapshot()，一处全覆盖。
- 教训：**「快照成功」不等于「镜像可用」**——验收要看渲染出的内容是否等价，不能只看通道有没有数据；对序列化路线先天拿不到的页面类别（特权页/shadow 重仓页），正确动作是明确降级到像素路并告诉用户，而不是端出残页。

### 38. 拿 /observe 当轮询判据会按 §7.1 把自己要验的人手势锁死（2026-11-20）

- 症状：新 IT 用例用「轮询 /observe 文本」当断言判据后，视频/DOM 两路双击恒失败；探针里也复现「observe 之后点击全灭、reload 后恢复、再 observe 再全灭」。
- 定因：observe 本身就是 AI tools/call——按 §7.1 交互模型，其在途期间及返回后保持窗（默认 2 秒）内，人通道手势被遮蔽（masked，等用户确认接管才放行）。400ms 间隔的轮询让保持窗永远续期，人手势被自己的判据无限期锁死。边缘侧探针（dbg54）同证：observe 后边缘 WS 点击页面零事件，而同一页面上探针自己的 CDP 会话点击全套正常——不是浏览器坏了，是仲裁在按设计工作。
- 解法：人路径的验收判据走人通道自己的回流（DOM 快照/差分、帧流），不要用 AI 通道调用当轮询器；真要交错调用，先等保持窗过或走接管确认。
- 教训：**观测手段会改变被观测系统时，判据本身要进设计评审**——尤其自家协议里带状态语义的调用（遮蔽/闩锁/代次），拿来当无副作用的读数是隐形地雷。

### 39. screencast 有两种帧：整页帧与「视频区帧」，混当整页会让编码器尺寸追逐（2026-11-20）

- 症状：LIU 在 Bilibili 看视频（视频模式/RTC）：画面不停在「铺满」与「变小」之间跳。远端页面本身纹丝不动（探针采样播放器尺寸恒定），跳的是观看端视频元素——RTC 流的分辨率在 1874×1082 与 1336×752/1302×752 之间来回换。生产日志一晚 `[rtc] 编码器就绪` 468 次，两个尺寸近 1:1 交替。
- 定因：dbg63 自有 CDP 会话对同一页面开 screencast 实测：帧有两种——整页帧（1875×1078，约 2/3）与只有 video 元素画面的「视频区帧」（1337×752/1308×752，约 1/3；1337×752 正是播放器视频区尺寸），且元数据 deviceWidth/Height 如实报小尺寸、offsetTop=0，没有任何「这是局部帧」的标志位。帧泵旧实现一律当整页帧存 last_rgb，RTC/HLS 编码层以 last_rgb 尺寸为准、尺寸一变就重建编码器（encode_raw 对尺寸不符直接报错，想不重建都不行）——流分辨率随帧型翻动，观看端 <video> 跟着忽满忽小。整页帧自身还有几像素的高度变体（1078/1055），同样触发偶发重建。
- 解法：帧泵按尺寸分流——以「见过的最大整页尺寸」为基准，宽高都达基准 90% 的是整页帧（透传 + RGB 归一化到基准尺寸，消掉高度变体）；更小的是内容帧：不广播、不动 last_frame，解码后按 video/canvas 的视口矩形（CDP 查、按内容尺寸缓存、页面滚动失效）贴回最近整页 RGB。编码层从此只见恒定尺寸。修复后同场景 45 秒编码器只建 2 次（初始默认视口 + 跟随放大各一次，皆真实尺寸变化），vidWidth 全程恒定。
- 教训：**上游帧流的「一帧」不等于「一整页」**——消费任何截帧/编码管线前，先对帧尺寸做一次普查（分布 + 元数据对照），尺寸分布多峰就是管线里藏着帧型差异；编码器这类有状态组件的尺寸基准必须来自视口这类稳定量，不能来自逐帧内容。

### 40. 镜像子资源被图片 CDN 按 Referer 防盗链拦截：no-referrer 解（2026-11-20）

- 症状：DOM 模式下 Bilibili 其它视频的缩略图全不显示（镜像 35 张图只加载 3 张），远端页自身 32 张加载 24 张（其余是懒加载未触发）。
- 定因：观看页里逐个看失败请求——hdslb.com 全部 requestfailed。同一 URL 在观看页上下文做对照加载：默认 Referer（=观看页源）→ err；`referrerpolicy="no-referrer"` → ok；`origin` → ok。hdslb 按外来 Referer 防盗链；远端页带 bilibili 自己的 Referer 所以正常。镜像文档的源是观看页，子资源天然带错 Referer。
- 解法：镜像 head 在建树接入前注入 `<meta name="referrer" content="no-referrer">`（必须赶在树里的图片开始加载之前——applySnapshot 本来就在 replaceChild 前组装 head，顺手加）。修复后镜像 36 张加载 31 张。
- 教训：镜像类「把别人的页面搬进自己的源」方案里，子资源的 Referer 永远是搬运者的源、不是原站的源；遇到「图全灭但 URL 都对」先查 Referer 防盗链，别先怀疑 URL 绝对化。伪造原站 Referer 做不到（浏览器禁改），no-referrer 是标准解，但对「无 Referer 也拦」的少数严格站点会反效果——那种站点只能走边缘代理，不在本条范围。

### 41. 镜像按舞台宽度排版 = 与远端布局系统性错位：布局视口必须等于远端视口（2026-11-20）

- 症状：DOM 模式点 Bilibili 进度条，跳转位置恒在点击处左边 100–150px。
- 定因：量测链完整闭环——镜像 iframe 旧样式是填满舞台（舞台 CSS 宽 1500），镜像文档就按 1500 排版；而远端视口 = 舞台 × 观看端 DPR（1.25）= 1875，远端按 1875 排版。两边响应式断点不同、且序列化把远端布局的固定像素尺寸（如播放器宽 1313）冻进了行内样式，镜像里播放器被迫溢出（实测镜像内进度条 x=−96、远端 x=53）。镜像 mousedown 的 pos() 直送 clientX（镜像布局坐标），在两套布局之间没有任何换算能救——布局本身就不一致。DPR=1 的对照轮里两边同宽、点击精确命中，反证成立。
- 解法：iframe 布局视口钉为快照的远端视口（m.w/m.h），整体以 CSS transform 等比缩放（letterbox）进舞台。布局同构之后 clientX 即远端布局坐标，pos() 一行不用改；要改的只有视觉换算两处（幽灵指针、右键菜单位置）：getBoundingClientRect 是缩放后矩形，布局坐标差须乘缩放比 r.width/mirVW。修复后两 DPR 实测偏差同为 +7px（残差来自远端页有滚动条、镜像滚动条占位不同，居中容器半个滚动条宽，属固有量级）。
- 教训：**镜像的坐标问题先问布局是否同构，再问换算公式**——布局不同构时，任何坐标换算都是在两套不同的页面之间猜映射；让镜像在与远端相同的视口里排版，坐标系天然合一，剩下的只有均匀缩放。

### 42. 进一次 DOM 再回视频就没声：影音开关不恢复 + 静音标志没有收敛点（2026-11-20）

- 症状：LIU 第五轮实测：视频模式没声音，观看页标签上没有喇叭图标（页面根本没在出声）。同链路探针（干净夹具 + Bilibili 页）却全程有声：远端页 audioDecoded 持续增长、rigsink monitor 有电平、RTC 音频 ~300 字节/包、观看端渲染电平 0.82。
- 定因：在观看端状态机里。domStart 为互斥会 POST /channels 关掉边缘影音并把 vid 静音，但三条退出路径（domStop/exitDomDisplay/domFallback）都不恢复：边缘影音保持关闭，本地 avOn=false、avVia=null；而 vid 的静音只在 pollAv 的两个迁移分支里解除（要求 !avOn 或 !avVia 的状态跃迁），往返之后 avVia 的定型路径再也不会被触发，vid 恒停在 ontrack 时的初始 muted=true。DOM 优先的使用顺序（先验 DOM 再回视频）必中。
- 解法：domStart 关影音前记 avWasOn，退出路径统一 restoreAvAfterDom 把边缘影音开回去（本地状态留给 pollAv 既有分支定型解静音）；同时把静音态收敛进 showDisplay 一处：DOM 面显示→静音，视频面显示且 avVia 已定型→解静音，任何迁移顺序最终都收敛到与显示面一致的状态。探针 dbg67 实测往返后 avOn/avVia 恢复、muted=false、渲染电平 0.21。
- 教训：**成对的开关（关必有开）要落在同一条恢复路径上；布尔状态不要散落在迁移分支里靠触发顺序维护，能收敛到「由当前状态推导」的一处就不要靠事件补丁**。另：第一版修复里给 pollAv 加的「DOM 期间 av 仍关着就清 avWasOn」守卫，用 6 秒 pending 窗区分「自己关的」和「用户关的」——DOM 停留超过 6 秒必误清、修复被自己的守卫废掉；区分不了的两种来源就不要猜，宁可保留恢复语义。

### 43. 视口尺寸横跳追进编码器：观看端要安定滞后，帧泵要认当前视口（2026-11-20）

- 症状：帧分类（§39）修好后 LIU 仍报视频模式跳；生产日志显示 RTC 编码器在 1280×656 与 1270×804 两尺寸间交替重建多次（单观看端受控复测同尺寸则恒定）。
- 定因：两层。(1) reportSize 只有 250ms 防抖：窗口拖动/状态切换期间的每个中间尺寸都被报给边缘，远端视口、截帧与编码器逐个跟随翻动。(2) 帧泵的整页基准只增不减（同会话内）：视口真缩小后的整页帧小于基准 90%，被误判为内容帧贴回旧尺寸底图——画面停在旧尺寸错位合成（生产中因视图切换重置基准而表现为编码器在两尺寸间重建）。
- 解法：观看端 reportSize 改为首报立即、其后 900ms 安定滞后（尺寸稳定才发、与已发值相同跳过）——探针 8 次 700ms 间隔交替改尺寸，编码器只在最终安定尺寸重建 1 次。帧泵分类前加当前视口对照：帧尺寸恰等于页面 innerWidth/Height（DSF 恒 1 同口径，500ms 缓存）即按新尺寸重定基准走整页路径。
- 教训：**跟随型尺寸链（窗口→视口→截帧→编码）每一级都要有阻尼**：源头用安定滞后过滤瞬态，消费端用权威量（当前视口）校正分类基准；只在中间某一级打补丁，瞬态会从另一级漏进来。

### 44. 探针夹具自己没信号：下游全静音先验源头（2026-11-20）

- 症状：查「没声音」时，探针一度得出「Chromium 根本不解码音频」的结论（audioDecodedByteCount 恒 0、monitor 全零），险些去改启动参数/Xvfb/asound。
- 定因：自家测试页 vid2.html 有三重污染——两个 video 元素都带 `muted` 属性；querySelector('video') 命中的第一个是 sample60.webm，其音轨是封进 WebM 的 AAC（Chromium 不解，源头本就无声）；且页面忽略 ?vm= 参数、src 写死。测到的「全程静音」是夹具的真实输出，不是被测链路的故障。连带第二个测量陷阱：观看端用 createMediaElementSource 测 MediaStream（RTC）电平恒为 0——该 API 对 srcObject 流不回传样本，要用 createMediaStreamSource；还有 tone.html 无 autoplay 属性，不经脚本按播放视频恒暂停，dbg67 首跑的「静音」同样是源头没播。
- 解法：换干净夹具（单 video、无 muted、tone30.mp4 正弦音）后全程实测：源头 audioDecoded 3 万+、monitor 峰值 4161、RTC ~300 字节/包、观看端电平 0.13–0.82，链路无故障，真因回到 §42。
- 教训：**探针先自证：夹具必须在源头产生可测信号（audioDecoded>0、monitor 有电平），测量点本身也要用已知有信号的输入校准过**。下游一片静默时，先查夹具和测法，再查被测系统——本轮在错误结论上差点展开三条改造线（启动参数、Xvfb、有声桥接），全靠回头验夹具拦住。

### 45. 恢复路径修好了，状态还是卡死：边缘 AV 恒关且全系统无人自愈（2026-11-20）

- 症状：§42 修复部署后 LIU 第六轮仍报没声音（DOM、视频都沒）。查生产 `/channels`：`av:false`、`av_available:true`——边缘影音处于关闭态，而系统里没有任何一环会把它打开。
- 定因：§42 修的是「同一次页面会话内」的往返恢复，但关 AV 的持久效应在边缘侧：domStart 的 POST 把边缘 av 关掉后，只要恢复代码没在同一个页面生命周期里跑到（旧版缓存页、标签页在 DOM 期间被关、bfcache 恢复的页面跳过初始化），边缘就恒关。观看页 pollAv 旧逻辑只对 `av:true` 做反应、从不主动开；**观看页菜单里根本没有 AV 开关**（只有 AI/人通道与分辨率）——用户在界面上没有任何办法把声音开回来，只能等边缘重启。探针环境每次都是新边缘（av 初始开），所以第五轮的往返探针测不到这条持久卡死。
- 解法：pollAv 加兜底自愈分支——视频模式（非 DOM、无待落定关 AV 窗口）下见到 `av:false` 且 `av_available` 就 POST 开回去，2 秒轮询内收敛；DOM 模式由 domOn/domAvOffPending 两道闸守住、不与互斥打架。dbg69 实测：外部强关后 8 秒内自愈且音频流量恢复满速；DOM 期间保持关；退出后恢复。同时观看页与脚本响应加 `Cache-Control: no-store`（此前无任何缓存头），杜绝旧版观看页被浏览器缓存续命。
- 教训：**跨端持久状态的开关，恢复不能只依赖发起方同会话的善后**：任何一端都可能中途消失。守恒式做法是让状态的巡检者（这里是 pollAv）带一个「应然状态」的兜底纠正，并先确认用户侧到底有没有手动恢复的入口——没有入口的卡死态就是设计缺陷，不是个案。

## 六、探针与测试方法论（栽过的跟头）

- **坐标换算**必须用真实帧尺寸且等画布收敛（宽 >900 且连续两次采样一致、帧宽=视口宽、条带差 0 或 88 二选一稳定）；reload 后有中间尺寸陈旧帧，只看显示矩形会算出负坐标。
- **点击成败以远端状态为证**（activeElement、计数器、paused 标志），不看坐标自洽。
- 进 DOM 模式**点真 #domBtn**，不裸发 dom-start：观看端有本地状态门控，裸发消息会被它自己丢弃，造成「边缘没发」的假象。
- **隔离归因三件套**：CDP 直打（远端能力）→ 原始 WS 往返（边缘协议）→ 观看端真实手势（全链路），逐层定位，不要跨层猜。
- **探针原点必须与用户实测一致**（局域网 IP 非安全上下文）：127.0.0.1 + 授权限会假通过（Ctrl+C 曾因此误报已通）。
- 测试页自身要合规范：@import 必须位于样式表首行才有效（曾误判序列化器）；样本视频太短会自然播完（误判暂停）；每轮总验前刷新远端页清状态（选区/文本污染 B/H 阶段）。
- **先验测试素材的属性再下结论**：样本视频的帧率（15fps 冒充管线上限，见 §27 钳三）、测试页里元素的 id 是否真存在（凭记忆写 #name，实际页面只有 #ta/#vid/#xf，整段 B 探针白跑）——素材属性用 ffprobe/grep 当场核实，不要凭印象。**素材先确认远端真有这份文件**（2026-11-20 dbg39：input-test2.html 只在本地、远端 /tmp/domspike 没有，边缘开的是 http.server 的 404 页，整轮探针在错误页上空转；本地与远端同名目录的 ls 输出并排出现时尤其容易看串）。
- **探针间状态隔离**：共用同一长命边缘页面的先后探针会互相污染（dbg39 在真页输入框留下粘贴文本，dbg39c 读到「双倍内容」险些误判边缘双写）——每轮探针用全新边缘/页面，或开始先读并记录基线值再比增量。
- **坐标换算要认当前显示元素**：RTC 连通后观看页显示的是 `<video>` 不是 canvas（canvas 塌成 0×0）——按 canvas 算坐标会把所有点击送到 (0,0)，探针还得出「菜单能开」的假阳性。换算函数取当前可见元素（vid 优先）的 rect 与内在尺寸（dbg25 教训）。
- CDP connectOverCDP 取远端端口：pgrep 用括号写法防自匹配、按 profile 名 `rig-edge-<边缘PID>` 过滤；边缘 PID 以 ss 监听为准（见 §4）。
- **pkill 括号防自匹配只准括首字符**：`[r]ig-edge-123` 才是字面匹配；把整串变量括起来（`"[$G]"`）得到的是**字符类**，会命中参数里含任一该字符的几乎所有进程（2026-11-20 第四期清理时实踩，险些误杀在役实例，靠 ss/health 复核才确认无损）。批量清理先按 ss 核实 PID 精确 kill 边缘本体，再按完整 profile 名逐个清 Chromium 组。**且 pkill 与含目标名的启动命令绝不同批/同行**：模式串出现在同一复合命令行的其他位置（后面的启动命令）照样自匹配自杀（第五期实证，ssh exit 255）；杀与启分两步走。
- Node 24 有全局 WebSocket，WS 探针不必 require('ws')；Playwright 的 evaluate 读顶层 let 绑定要用裸标识符。

## 七、借鉴与未采纳（参考 Go 实现 personal-agent，2026-11-20 研读）

- 已采纳：剪贴板显式请求/回应模型（§7）、右键菜单观看端绘制（§11）。
- 未采纳（记待议）：**字符归文本键盘模型**（无修饰普通字符走 input 事件整段送 text，IME 类问题从源头消失）——RIG 现行桌面打字路径已验证可用，重构回归风险大，暂缓；若日后输入法问题复发再议。
- 未采纳（结构性差异）：帧流零解码透传 + 「观看端取走才 ack」背压——RIG 因 chrome 合成必须解码重编码，多一层是结构性的；真要优化先量
...[truncated 2119 chars]