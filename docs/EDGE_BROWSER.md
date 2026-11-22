# rig-edge-browser —— 瀏覽器邊緣程序說明

> 本文是 `crates/rig-edge-browser` 的權威使用與實現說明，供後續開發與外部 review 使用。
> 定位见 `README.md` 与 `CLAUDE.md`；感知語義細節見 `docs/PERCEPTION.md`；
> 協議型別（Observation / Action / ActionRequest 等）定義在 `crates/rig-proto`。

## 1. 它是什麼

RIG 的**瀏覽器邊緣程序**：一個 Rust 二進位，與目標 Chromium 同機（或同級）部署，
把一個真實瀏覽器同時開放給兩類消費者——

- **AI 通道**：把頁面壓成緊湊的無障礙（AX）語義快照供智能體觀察，並以「幀號 + 元素 id」
  執行帶陳舊校驗的動作。語義優先；無語義區域（canvas 等）以編號 PNG 截圖兜底。
- **人通道**：把瀏覽器畫面（及聲音）流給人看，並把人的滑鼠/鍵盤/剪貼板輸入送回頁面。

邊緣程序是**直連本體**：本機或局域網即可獨立使用。經 Gateway 中轉只是可選的轉運
通道（反向接入尚未實現，見 §11），不是使用前提。轉發中心永不抓取、渲染或解析——
所有感知與呈現都發生在邊緣。

**定位定版（LIU 2026-11-19）**：本子項目自身即是完整產品——一個可獨立交付、
獨立驗收的瀏覽器控制端，人與 AI 智能體直連即可用（形態見 §1.1）。
受控端反向接入 Gateway 轉發是**擴展能力**（解決跨網路可達性），排在邊緣
功能打磨之後；本子項目的任何功能、測試與發布都不以 Gateway 存在為前提。

### 1.1 不經 Gateway 的完整用法（快速開始）

| 消費者 | 形態 | 做法 |
|---|---|---|
| 人（同機） | 觀看頁 | `rig-edge-browser --url https://example.com` 後開 `http://127.0.0.1:39871/`：視頻/DOM 雙模式觀看與操作、原生 header、音視頻（HLS/RTC）都在這一頁 |
| AI（同機） | MCP stdio | 智能體以子進程拉起 `rig-edge-browser --stdio`，JSON-RPC 協議見 §7；observe/act/screenshot 三工具 |
| AI（局域網/遠程直連） | HTTP | `rig-edge-browser --listen 0.0.0.0:39871 --token <令牌>`（非回環無令牌拒絕啟動，見 §6）；`POST /observe`、`/act`、`/screenshot.png`（Bearer 啟動令牌） |
| 人+AI 協作 | 同時在線 | AI 通道與人通道可並存：交互模型（待命/執行/強制接管）見 §7.1，人隨時可接管 |

四種形態共用同一份感知與動作語義（§10），差別只在傳輸與前端外殼。

## 2. 架構與模組

```
                ┌─────────────── rig-edge-browser ───────────────┐
  AI 智能體 ────►  HTTP /observe /act  ┐                          │
  (同機/局域網)    MCP stdio (JSON-RPC) ┼─► ai.rs  AiChannel ─┐    │
                                        │                     │    │
  人 ───────────►  觀看頁 / + WS        ┼─► human.rs HumanChannel ─┼─► browser.rs
                                        │                     │    │   (Chromium 託管/掛靠)
  人 ───────────►  /stream (HLS)        ┴─► media.rs MediaPipeline ┘    │
                                                            cdp.rs (手寫 CDP 客戶端)
                                                            snapshot.rs (AX 快照引擎)
```

| 模組 | 職責 |
|---|---|
| `cdp.rs` | 手寫 CDP 客戶端：WebSocket 幀、flatten 多會話（`Target.attachToTarget {flatten:true}`）、請求/事件多路複用 |
| `browser.rs` | Chromium 生命週期：自拉起（launch）或掛靠（attach）；頁面會話管理、截圖、鍵鼠原語、剪貼板授權 |
| `snapshot.rs` | AX 快照引擎：全量樹遍歷、OOPIF 子幀掛接與坐標合成、ignored 透傳、generic 殼剪枝、穩定 id 分配、密碼子樹脫敏 |
| `ai.rs` | AI 通道：observe（生成快照幀）/ act（陳舊校驗後執行 Action）/ 編號截圖 |
| `human.rs` | 人通道：按需求啟停 CDP screencast、輸入事件注入、文本/圖片剪貼板橋 |
| `media.rs` | 音視頻合流：PulseAudio 虛擬聲卡 + 進程內 libav 雙版本 HLS（見 §8） |
| `encode.rs` | 進程內編碼層：libavcodec + hw_frames_ctx 運行時协商（硬編優先、軟編回落） |
| `chrome.rs` | 瀏覽器 chrome 狀態：標籤/導航/尋找/解析度/通道閘門的狀態保存、`chrome-state` 推送 JSON 與地址輸入歸一化（觀看端原生 header 的數據源，見 §9.1） |
| `exec.rs` | 執行狀態機：待命/執行/保持窗、AI 幽靈指針軌跡與意圖、強制接管中斷（見 §7.1） |
| `rtc.rs` | WebRTC 主路：str0m offerer、共享 feeder 一次編碼多會話扇出、PLI/BWE 回寫、P2P srflx 採集（見 §9.1） |
| `dom.rs` | DOM 鏡像通道：頁內序列化器注入、快照/差分轉發、重同步觸發（見 §9.2） |
| `main.rs` | axum 服務裝配、通道閘門、令牌鑑權中介層、觀看頁（`viewer.html`/`viewer.js` 以 include_str 嵌入，R-17）、MCP stdio 前端 |

## 3. 瀏覽器的兩種來源

| 模式 | 觸發 | 行為 | 生命週期歸屬 |
|---|---|---|---|
| 自拉起 launch | 預設 | 在系統/Playwright 安裝中自動發現 Chromium，以獨立 profile 拉起（沙箱預設保留；僅 root 環境——Chromium 在 root 下無法啟用沙箱——或顯式設 `RIG_CHROMIUM_NO_SANDBOX=1` 時才加 `--no-sandbox`） | 邊緣擁有：退出時關閉瀏覽器 |
| 掛靠 attach | `--cdp-url http://127.0.0.1:9222` | 輪詢 `{base}/json/version` 取 `webSocketDebuggerUrl` 後連接現成 Chromium | 瀏覽器自有：邊緣退出**只斷開連接，絕不關閉被掛靠的瀏覽器**（集成測試驗證） |

兩種模式連接後的頁面會話建立、AX 啟用、剪貼板授權流程完全一致（內部共用
`finish_connect`）。

**UA 規範化（2026-11-20 補）**：`finish_connect` 在掛載任何頁面會話前，
以 `Browser.getVersion` 取本體 UA，把 `HeadlessChrome` 替換為 `Chrome`
（自拉起的 headless-shell UA 帶 Headless 字樣，實測觸發 YouTube 機器人
牆、播放器不初始化——PITFALLS §21），並對每個 page 會話設
`Emulation.setUserAgentOverride`；環境變數 `RIG_EDGE_UA` 給出時整串
覆蓋優先。

**人格補完（2026-11-20 第二批，LIU 定向：給人與 AI 用的瀏覽器不
保留 headless/CDP 開發形態的痕跡）**：指紋電池（dbg24）實測基線
後逐項根治，全部只對自拉瀏覽器生效，掛靠真實瀏覽器整段跳過：
① UA 覆蓋必須連帶 `userAgentMetadata`（brands/fullVersionList/
platform/arch——版本取 product 的真實完整版本，UA 串裡的是約簡
版），只改串會讓 `navigator.userAgentData` 高熵值整體清空，空
元數據本身就是異常信號；② 時區 `Emulation.setTimezoneOverride`
（`RIG_EDGE_TZ`，默認 Asia/Tokyo）與語言（`--lang` +
覆蓋的 `acceptLanguage`，`RIG_EDGE_LANG`，默認 ja-JP；acceptLanguage
不帶 q 值——會被原樣拆進 `navigator.languages`）；③ 啟動參數去掉
`--disable-gpu` 並加 `--enable-unsafe-swiftshader`：前者讓 WebGL
整體不可用（硬信號＋弄壞站點），無 GPU 主機自動回落 SwiftShader
軟件渲染；④ `setDeviceMetricsOverride` 連帶 screenWidth/Height
（虛擬屏默認 800×600 比窗口還小是物理不可能的異常）。基線本就
乾淨的：webdriver=false（邊緣不帶自動化標誌啟動）、plugins 5 項
標準 PDF、window.chrome/loadTimes 在、CDP console 探針不觸發
（邊緣不啟用 Console 域）。殘留（已知、未處理）：WebGL renderer
字串含 SwiftShader（VM/遠程桌面同款，非無頭專屬）；DOM 模式用
過的頁面留 `__rigDom`/`__rigSend` 全局（新頁面/新 profile 乾淨）；
出口 IP 地理與人格時區不一致屬部署層。更深的處理（改 Chromium
源碼自編譯）成本另議，見 PITFALLS §25。

## 4. 命令行參數

| 參數 | 預設 | 說明 |
|---|---|---|
| `--listen <addr:port>` | `127.0.0.1:39871` | HTTP 服務綁定位址（stdio 模式下僅 `--human` 時用於觀看頁） |
| `--url <url>` | `about:blank` | 啟動後導航到的頁面；其 origin 自動進入導航白名單 |
| `--cdp-url <base>` | — | 掛靠模式：現成 Chromium 的 DevTools HTTP 基位址（見 §3） |
| `--chromium <path>` | 自動發現 | 自拉起模式的 Chromium 可執行檔路徑；也可用環境變數 `RIG_CHROMIUM` |
| `--width` / `--height` | 1280 / 800 | 瀏覽器視口尺寸 |
| `--allow-origin <origin>` | — | 追加允許 Navigate 的 origin（可重複給多次） |
| `--no-ai` | — | AI 通道預設關閉（運行時仍可經 `/channels` 開啟） |
| `--human` | — | 人通道預設開啟（不給則預設關閉，觀看頁無畫面） |
| `--av` | — | 預設允許 HLS 播放，首個有效播放列表請求啟動合流 |
| `--token <secret>` | 環境變數 `RIG_EDGE_TOKEN` | **對外提供 HTTP 時必設**（空值視同未設），由啟動程序隨機生成並傳入；見 §6 |
| `--embed-origin <origin[,…]>` | — | 允許內嵌觀看頁的宿主來源；未給則禁止任何內嵌（見 §6.1） |
| `--tls-cert <path>` / `--tls-key <path>` | — | HTTPS 服務（須成對給）；檔案缺失時自動生成自簽名證書（SAN 含 localhost、127.0.0.1 與綁定 host），私鑰以 0600 原子建立落盤復用。**非回環監聽強制 TLS**（LIU 2026-11-20 裁定，整改第二期）：未給證書時自動生成自簽名（`~/.rig-edge/self-signed.{crt,key}`）並顯著提示——DOM 通道承載頁面欄位值，其安全性由傳輸加密保證（R-07 處置）；僅回環監聽可明文 |
| `--stdio` | — | MCP stdio 前端模式（見 §7） |
| `--profile-dir <path>` | 環境變數 `RIG_EDGE_PROFILE_DIR`；未給 = 一次性臨時目錄 | **持久化目錄**（LIU 2026-11-20 定）：Chromium 用戶數據目錄。登入態、Cookie、LocalStorage / IndexedDB / Session Storage、Service Worker 與 Cache Storage、保存的密碼、擴展、已安裝應用、站點權限全在其中——邊緣重啟原樣續用，**整目錄搬到新環境重部署即可無損打開之前的網站**。密碼存儲固定 `--password-store=basic`：OSCrypt 密鑰隨 profile 的 Local State 走，新機器可解密；用宿主 keyring 則密鑰不跟目錄走、遷移即全滅。同一目錄同一時間只能被一個邊緣使用（Chromium 單例鎖）。PWAs 的系統層快捷方式不在目錄內、需在新環境重建，應用數據本身隨目錄走 |
| `--cache-dir <path>` | 環境變數 `RIG_EDGE_CACHE_DIR`；未給 = profile 內 | 可棄緩存目錄：Chromium 磁盤緩存（HTTP / 媒體）經 `--disk-cache-dir` 放到這裡，與 profile 分開存、遷移時可不帶。GPU / 著色器等小緩存無官方分離參數、仍留在 profile 內（體積小、可自愈重建） |

**臨時目錄清理**（LIU 2026-11-22）：未專門指定的目錄（沒給 `--profile-dir` 的一次性 profile `<tmp>/rig-edge-<pid>`、沒給 `--cache-dir` 的上傳目錄 `<tmp>/rig-edge-<pid>-uploads`）在 Chromium 關閉**之後**於退出時刪除（HTTP / stdio 兩模式、SIGINT / SIGTERM / stdin 關閉同）；專門指定的目錄一律不動。SIGKILL 與崩潰無法兜底，會留下殘餘。

持久化與停機口徑：邊緣收到 SIGINT / SIGTERM 時先請 Chromium 以 `Browser.close` **落盤關閉**再退出（HTTP 與 stdio 兩模式同）——強殺（SIGKILL）會丟掉最近未提交的 Cookie / Local Storage 寫入（IT 實測：數秒前的寫入被整組強殺即丟失，IndexedDB 因事務即時提交幸免）。部署側停機請用 TERM 而非 KILL。集成測試 `edge_profile_persistence` 驗證：寫入 Cookie / LocalStorage / IndexedDB → SIGTERM 優雅停機（附落盤實據斷言）→ 同雙目錄重啟 → 三值全數恢復。

## 5. HTTP 前端

| 路由 | 方法 | 說明 |
|---|---|---|
| `/` | GET | 觀看頁（人通道 UI：原生 header、畫面、輸入覆蓋層、通道開關、影音流入口） |
| `/viewer.js` | GET | 觀看頁腳本（與 `/` 同屬 R-17 拆分；經鑑權中介層） |
| `/health` | GET | 存活探針（免鑑權）；回 `{"ok":true,...}`，整改第四期起附可觀測字段：`ready`、`cdp_connected`、`last_frame_age_ms`、`exec_in_flight`、`frames{sent,skipped}`、`latency{compose,encode,act,observe,dom_snapshot,quads}`（各分段 p50/p95/p99/max + 累計）、`last_snapshot`（最近一次 observe 的總時/CDP 調用數/quads 次數與耗時/AX 段耗時/節點數） |
| `/ready` | GET | 就緒探針（免鑑權、無敏感信息）：CDP 存活且至少一個會話 → 200，否則 503。與 /health（liveness）分離（R-19） |
| `/channels` | GET | 查三通道狀態：`{"ai":bool,"human":bool,"av":bool,"av_available":bool}` |
| `/channels` | POST | 設通道開關，請求體同上形狀（部分欄位可省略） |
| `/observe` | POST | AI 觀察：請求體為 rig-proto 的 `ObserveRequest`（可空 `{}`）。回應組成由 `mode` 定：`text` 只回 `{"text"}`；`tree`（默認）回 `{"text", "observation"}`；`tree_image` 再加 `"image_png_base64"`。`budget_tokens` 按預算裁剪**響應視圖**（存儲幀不動、被裁元素憑 id 仍可操作，裁剪說明附在 text 末尾）；`subtree` 只回某元素子樹（id 不存在回 400）。`include_offscreen` 默認 false：視口外元素不進響應視圖（存儲幀保留、帶 Offscreen 狀態），true 才帶。`since_frame` 使用客戶端最後套用的觀察幀。當 `mode=tree` 且分頁、文件、座標空間與輸出投影相容時，可回 `{"delta": ObservationDelta, "text"}`。基幀須在最近 8 個已輸出的結構化觀察內。無法重建目前視圖時回整幀 |
| `/act` | POST | AI 動作：接受 `ActionRequest`，回 `{"result": ActionResult}`。頁面狀態類拒絕另附最新觀察及文字，或附無法取得最新狀態的說明 |
| `/screenshot.png` | GET | 當前頁編號截圖（PNG；敏感元素區域黑塊遮罩） |
| `/ws` | GET→WS | 人通道：下行 screencast 幀與狀態；上行輸入事件（見 §9） |
| `/stream/{rend}/{name}` | GET | HLS 播放列表與分片。有效列表請求可啟動合流，12 秒沒有有效請求即回收 |

**通道閘門語義**：AI 關閉時 `/observe`、`/act`、`/screenshot.png` 回 409；
人通道關閉時會斷開既有觀看連線，停止輸入並釋放 DOM、RTC 與 JPEG 需求。
從 `/channels` 重開後，觀看端需重新連線。AV 關閉時合流管線停機。三通道全關時僅控制介面可用。
閘門在 HTTP 與 MCP 兩個前端一致生效。

## 6. 直連鑑權（LIU 2026-11-22 重做，見 `docs/HARDENING_PLAN_2026-11-22.md`）

- **啟動令牌必設**：只要對外提供 HTTP（純 HTTP 模式，或 `--stdio --human`），
  `--token` / `RIG_EDGE_TOKEN` 缺失或為空即以退出碼 2 拒絕啟動。令牌由**啟動程序隨機生成並傳入**，
  換令牌 = 換值重啟。非回環監聽強制 TLS（未給證書則自簽）。
- **令牌只走 `Authorization: Bearer` 頭與換票請求的 body，URL 中不再接受任何令牌。**
- **路由拆分**：
  - Viewer 路由：`/`、`/viewer.js`、`/auth.js`、`/health`、`/ready`、`POST /session`、
    `POST /session/refresh`、`/channels`、`/print.pdf`、`/stream/*`、`/ws`。
  - AI 路由：`/observe`、`/act`、`/screenshot.png`，**僅純 HTTP 模式掛載**；
    `--stdio` 模式下 AI 只能走 MCP，本機其他進程無法經 HTTP 繞過 AI 閘門。
- **換票**：`POST /session` body `{"token": <啟動令牌>}` → `access`（15 分鐘）+ `refresh`
  （無時限）+ `stream_key`。`POST /session/refresh` body `{"refresh": …}` 換新一對，
  **舊 refresh 立即作廢**（輪換，被盜用時真假雙方必有一方失效）。viewer 在 access 將過期時
  自動靜默續期，無登入時長。票據只存內存，**進程重啟即全部失效**。
- **憑證權限**：Viewer 路由認 access 票據（或啟動令牌）；AI 路由只認啟動令牌，access 不得調用。
- **WebSocket**：瀏覽器 WS 無法設請求頭，連上後須在 3 秒內以首幀
  `{"type":"auth","access":…}` 認證，否則斷開；非瀏覽器客戶端可在升級請求帶 Bearer 頭。
- **媒體流**：HLS 由原生 `<video src>` 請求，無法帶頭。`/stream/{stream_key}/{rend}/{name}` 用路徑中的
  隨機 `stream_key`（只讀、僅限 `/stream`，登入後才下發），播放列表的相對分片位址自然繼承該前綴。
  這是「令牌不入 URL」的唯一例外，且該鑰匙不具備任何控制能力。

### 6.1 觀看頁的兩種打開方式與第三方內嵌

- **獨立打開**：訪問 `https://host:port/`，顯示令牌輸入頁，輸入啟動令牌後進入。
- **內嵌 iframe**（智能體自身的 web UI）：
  1. 啟動邊緣時用 `--embed-origin https://agent.example`（逗號分隔多個）聲明允許的宿主來源；
     服務端據此設 `Content-Security-Policy: frame-ancestors …`，**未配置則禁止任何內嵌**。
  2. 宿主頁面嵌入 `<iframe src="https://edge-host:port/">`，iframe 載入後向父頁發
     `{type:"rig-ready"}`；宿主回覆 `iframe.contentWindow.postMessage({type:"rig-auth", token}, edgeOrigin)`，
     viewer 只接受來自 `window.parent` 的該消息，自動換票進入，無需人工操作。
  3. https 的宿主頁面不能內嵌 http 的 viewer（混合內容）；自簽證書需用戶先在瀏覽器信任一次。

## 7. MCP stdio 前端（提交 `4bf5d08`）

`--stdio` 時邊緣以 **MCP（Model Context Protocol）** 在 stdin/stdout 上說
行分隔 JSON-RPC，給與智能體同機部署的場景：智能體把邊緣當子進程拉起，
AI 操作全走 stdio；人要看畫面時加 `--human`，HTTP 觀看頁並行提供。

- **stdout 只寫協議幀**；所有提示與診斷走 stderr，不得污染協議流。
- 方法：`initialize`（協商 `protocolVersion`：客戶端版本在 2025-06-18／2025-03-26／2024-11-05 內則回同版本，否則回 2025-06-18；
  `serverInfo.name = "rig-edge-browser"`）、`ping`、`tools/list`、`tools/call`；
  無 `id` 的通知不回覆；未知方法回 `-32601`。
- 工具（與 HTTP 語義一一對應，共用同一 AiChannel 與通道閘門）：

| 工具 | 入參 | 回覆 |
|---|---|---|
| `observe` | `ObserveRequest`（可省略；`mode`/`budget_tokens`/`subtree`，語義同 HTTP /observe） | text 內容塊：緊湊文本渲染（+ Observation JSON，`mode=text` 時不帶）；`mode=tree_image` 再加 image 內容塊（編號截圖） |
| `act` | `ActionRequest` JSON（rig-proto） | `ActionResult` JSON 文本塊。頁面狀態類拒絕另附文字和 `Observation` 或 `ObservationDelta` JSON，如果無法採集則附診斷文字 |
| `screenshot` | 無 | image 內容塊：base64 PNG（編號截圖，同 HTTP 的遮罩規則） |

- **`request_id` 前端補全**：`ActionRequest.request_id` 是協議必填欄位，
  但 MCP 調用方已有 JSON-RPC `id` 標識本次呼叫——`act` 入參缺 `request_id`
  時由前端生成唯一值（`mcp-<pid>-<seq>`）後再交給通道。HTTP 前端不補，
  保持協議原樣嚴格。
- stdin EOF 即正常收尾（關閉自拉起的瀏覽器；掛靠模式僅斷開）。

### 7.1 人機交互模型：待命 / 執行 / 強制接管（LIU 2026-11-19 定，已實現）

前提共識：AI 的操作速度必然慢於人。因此人和 AI 的共存**不靠同時搶
同一頁面**，而靠狀態分工——AI 平時只連通不處理，人下達具體指令時
才短暫獨佔執行，人隨時可奪回。本模型同時適用 MCP 與 HTTP 兩個
AI 前端；下文的「AI 客戶端」指 MCP 客戶端或 HTTP 調用方。

**狀態機（AI 數據流通道）**：

| 狀態 | 含義 | 邊緣行為 |
|---|---|---|
| 未接入 | AI 通道關閉或客戶端未連接 | 無 |
| 待命 Idle | 已接入（授權完成），無指令在執行 | **不做任何識別處理**：不拍快照、不跑 AX、不消耗 token；AI 客戶端不得在待命期輪詢 observe |
| 執行 Executing | 客戶端已發出具體指令（任一 tools/call 在途） | 正常處理；人側進入遮蔽（見下） |

- 進入執行：任一 `tools/call` 到達即進入；在途 call 全部返回後，
  經**保持窗**（默認 2 秒，其間無新 call）回待命——保持窗覆蓋
  「一條用戶指令拆成多個 call」的間隔，避免遮蔽反覆閃爍。
- 執行期人側行為（WebUI）：顯示 **AI 特殊指針**（ghost pointer，
  見 §7.2），並啟用**輸入遮蔽層**——用戶對頁面的滑鼠/鍵盤輸入被
  攔截、不轉發給頁面。指令完成（回待命）即撤遮蔽、移除 AI 指針。

**AI 特殊指針（§7.2）**：觀看頁在畫面疊加層繪製，不注入頁面本身
（不污染頁面與截圖）。跟隨 AI 動作坐標移動並繪出軌跡（漸隱），
點擊呈波紋，旁註當前動作意圖（動作類型 + 目標元素名/id，如
「點擊 登入 [e5]」）——人看到的不只是 AI 點了哪，還有它想幹什麼。
實現口徑（第五期起，取代初版的幀合成繪入）：指針由邊緣經 WS
`ghost` 消息推送（位置 + 軌跡點列 + 意圖標籤），觀看頁在當前
顯示面（視頻/canvas/DOM 鏡像）上以疊加層繪製——三種模式坐標
各自映射、呈現一致。HLS 合流路不帶指針（其定位是兜底錄像式
呈現）。

**強制接管（§7.3）**：

- 遮蔽期間，用戶只要焦點在瀏覽頁面上點擊滑鼠或敲擊鍵盤，WebUI
  即彈出提示：是否強制接管 AI 操作。選**否**：關閉提示，遮蔽
  繼續（之後新的獨立手勢可再次觸發提示）。選**是**：WebUI 向
  邊緣發送接管控制消息（人通道 WS 上行 `takeover`）。
- 邊緣收到接管：① 立即停止在途 AI 動作的後續注入（動作執行在
  步間檢查中斷標誌）；② 在途 `tools/call` 以中斷結果返回
  （`ActionResult` 新增 `Aborted { reason: UserTakeover }`，
  rig-proto 變更點）；③ 向 AI 客戶端發送服務端通知
  `notifications/rig/control_taken_over`（含被中斷的請求標識與
  時間），明示「用戶強制接管控制，請停下操作」；④ 狀態回待命，
  撤遮蔽、移除 AI 指針。
- AI 客戶端收到中斷後應停止當前指令、等待用戶新指令；用戶可
  發布修正後的指令重新開始（新的 tools/call 重新進入執行）。
- 接管只中斷指令，不影響連接與授權：MCP 連接保持，token 不變。
- HTTP 前端無服務端推送：在途請求以同樣的 `Aborted` 結果返回
  即為中斷信號，不另發通知。

**實現狀態**：已實現（人通道補強包，與 B4 合併實施）。要點：

- 狀態機在 `exec.rs` 的 `ExecControl`：observe/act 兩前端共用
  begin/end 進出執行態；保持窗 2 秒以 generation 計數防抖，
  且**按在途計數觸發**——僅最後一個 call 結束才啟動保持窗
  （整改第一期 R-02；此前單個 call 先結束可提前回待命）。
  待命期邊緣不主動跑任何識別（observe 由客戶端調用才觸發）。
- 接管中斷：取消狀態按**代次（cancel_epoch）**管理——接管
  遞增代次，在途 call 領的是舊代次、永不被後來的新 call 復活
  （R-01；此前 begin 無條件清中斷標誌）。act 執行在每個副作用
  CDP 注入前檢查代次（R-03：click 的 press/release 之間、
  Type 的點選与輸入之間、press_key 的 down/up 之間均有檢查
  點；中斷時只補必要的釋放事件）；在途 call 回
  `Aborted { reason: UserTakeover }`。若接管發生在最後一步已
  落地之後，該 call 仍如實回 Aborted——動作可能已生效，客戶端
  應以新的 observe 為準，不要假設未執行。
- **接管闩鎖（整改第一期，LIU 口徑）**：接管後新的 act 一律
  直接回 Aborted，直到一次 observe 成功完成（客戶端已按新
  現實重新同步）才放行；observe 本身永不闩鎖。
- MCP 前端在被中斷的 tools/call 返回後，補發一行
  `notifications/rig/control_taken_over`（params 含 reason）；
  HTTP 前端以 Aborted 返回為準，無通知。
- B4 同批落地（`ai.rs`）：Observation 與 LocateMap 收進單一
  `FrameState` 同快照整體替換（幀原子性）；act 以串行鎖逐個
  執行；Element 目標先刷新再做跨幀同一性比對，Point 目標以
  存儲幀做幀號嚴格相等 + Space 越界校驗（只含 Point 的動作
  若先刷新會使幀號永遠過期，故不刷新）。

## 8. 音視頻合流（提交 `851a5b9`；Phase 1a 改進程內管線）

定案（LIU 2026-11-18）：**HEVC 為主輸出 + 同時編一份 H.264 兜底**，判據是
「客戶端無需安裝解碼器就能看」——Chromium 系 HEVC 僅硬解、無軟解，
無硬體支援的機器由觀看頁以 MediaSource 能力探測自動落到 H.264。

**管線形態（LIU 2026-11-19 定案，Phase 1a 落地）**：棄 stdio ffmpeg 子進程，
改 Rust 進程內直調 libav（ffmpeg-next 連結系統 libav* 庫）。理由：stdio 形態
有 JPEG 代際損耗、無編碼器反饋（PLI 強制關鍵幀做不了）、流控粗糙；進程內
編碼層（`encode.rs`）以 **hw_frames_ctx 運行時能力协商**——有 GPU/硬編則用
（候选序 NVENC→QSV→VAAPI→Vulkan），無則軟編回落，一份代碼不以開發機
（無 GPU 的 LXC）為設計基準。协商必須一路試到編碼器 open 成功為止：實測
Vulkan 軟渲染能建設備上下文但無 encode queue 擴展、編碼器打不開。
RTC 與 HLS 兩路共用此編碼層。運行時依賴系統 libavcodec/libavformat/
libavutil/libswscale/libavdevice/libswresample。

- 音頻：PulseAudio null sink `rigsink` 設為預設輸出，Chromium 啟動前以
  `PULSE_SINK=rigsink` 注入——聲音進虛擬聲卡，合流管線以 avdevice pulse
  輸入從 `rigsink.monitor` 進程內採集。pulse 流參數與解碼幀的聲道布局
  為空（mask=0）時須按聲道數歸一化為默認布局，否則 libswresample 逐幀
  以 Input changed 拒收；平面聲道數據須走 `plane::<f32>()` 讀寫（
  ffmpeg-next 的 `data(i)` 按 `linesize[i]` 取長，音頻幀 `linesize[1]==0`
  會返回空切片）。AAC 固定 1024 樣本/幀手動累積；音視頻共用時間原點
  （音頻樣本計數自首個合流器組建好≈首幀時刻起算），否則首段對位的是
  頁面發聲前的靜音。
- 視頻：頁面 RGB 幀直餵編碼器（無 JPEG 代際損耗），100ms 節拍、
  無新幀則重複最近幀，保證恆定 10fps。
- HLS 由有效播放列表請求啟動，無需 Viewer WebSocket。成功的列表與分片請求延長播放需求。
  `av_wanted` 表示允許播放，`av` 表示管線正在運行。RTC 連線狀態由各觀看端處理，直連 HLS 仍可獨立播放。
- 編碼：HEVC = libx265（veryfast、1.5Mbps、`hvc1` 標籤經 codecpar 寫入）；
  H.264 = libx264（veryfast、2.5Mbps）；關鍵幀間隔 20 幀（2 秒）；
  均 AAC 48kHz 128k。HLS fMP4：`hls_time 1`、列表 6 段、自動刪舊段；
  輸出於臨時目錄 `rig-edge-hls-<pid>/{hevc,h264}/`，布局與舊管線一致。
- **虛擬聲卡自愈**（`ensure_virtual_sink`）：所有 pactl/pulseaudio 呼叫帶
  10 秒超時（防僵死守護進程掛死啟動）；探活失敗時先 `--start`，仍不可達
  則 `--kill` 後重啟一次；最終以「sink 出現在列表中」為就緒判據。
  實測背景：殘留的 pulseaudio 守護進程會僵死（進程在、pactl 連接被終止），
  舊實現因此誤判 AV 不可用。

## 9. 人通道輸入與剪貼板

- **頁面鍵盤**使用原始 `key`、物理鍵位 `code`、左右及數字鍵盤位置、
  重複狀態和修飾鍵。可輸入的字元由一次 CDP 鍵事件寫入。Ctrl、Alt、Meta
  組合鍵保留其修飾鍵。AltGr 產生的字元直接插入，避免被 Chromium 當成 Ctrl+Alt 快捷鍵。
  IME 在觀看端組字，
  確認後只送一次完整文字。視窗失焦、呈現模式切換或斷線會釋放已送出的按鍵。
- **觀看頁快捷鍵**在畫面及 DOM 模式共用。Ctrl/Cmd+F 開遠端查找，
  Ctrl/Cmd+L 聚焦觀看頁地址欄，Alt+←／→ 控制遠端上一頁／下一頁。
  F5 或 Ctrl/Cmd+R 重載，Ctrl/Cmd+T／W 開啟／關閉遠端分頁，
  Ctrl/Cmd+PageUp／PageDown 切換遠端分頁。快捷鍵須由觀看者瀏覽器交給網頁，
  瀏覽器或作業系統優先攔截的組合可用觀看頁工具欄操作。
- **AI 通道按鍵**：`Action::Key` 的鍵名經 `key_spec` 映射為
  （key, 物理鍵位 code, text）：單字符鍵的 code 是 `KeyA`/`Digit1` 等物理鍵位，
  **不是字符本身**——曾誤用字符作 code，導致 Ctrl+A/C/V 等組合鍵全部失效
  （原始 CDP 事件探針定位）。按下事件用 `rawKeyDown` 並帶
  `windowsVirtualKeyCode`/`nativeVirtualKeyCode`。
- **文本剪貼板（顯式模型，2026-11-20 改版）**：複製是一次請求/回應——
  觀看端在 Ctrl+C/X 後發 `clipboard-get`，邊緣讀遠端**當前選區**
  （輸入框選區 → window 選區 → 遠端剪貼板兜底）回傳，觀看端寫本地
  剪貼板（安全上下文走 Async Clipboard API；失敗/非安全上下文彈
  手動複製框兜底）；貼上靠觀看頁真實 paste 事件直送。舊實現的
  「邊緣 700ms 輪詢同步 + 觀看端暫存補寫」已整刪——暫存補寫曾把
  使用者在外部的新複製覆蓋回舊值（事故與定因見 PITFALLS §7）。
  DOM 模式的複製以**鏡像自身選區**為準，不轉發遠端（PITFALLS §9）。
- **圖片剪貼板**：本地圖片 paste → 觀看頁轉 base64 → WS
  `paste-image` → 頁面以 ClipboardItem 寫入遠端剪貼板並發真實
  Ctrl+V；頁面內複製圖片兩路：右鍵菜單「複製圖片」（見 §9.1
  菜單）——視頻模式按幀座標取圖、DOM 模式按節點 id 取圖
  （座標取圖會被鏡像布局漂移坑，PITFALLS §12）；頁面自己把
  圖複製進剪貼板時（canvas/按鈕複製流），觀看端在 Ctrl+C 後
  順帶發 `clipboard-read-image` 顯式讀回（一次請求/回應，
  有圖才回），回傳後寫本地剪貼板。
- **文件粘貼未實現**（LIU 2026-11-18 定：文件單獨考慮）。正道是 rig-proto
  新增 Upload 動作走 `DOM.setFileInputFiles`——改協議須 LIU 批准。
- **右鍵選單**。地址欄和查找欄使用觀看者瀏覽器的原生編輯行為。
  頁面右鍵仍送到遠端 Chromium，並開啟觀看端選單，提供複製、貼上、
  全選、複製圖片、後退、前進、重載和自動模式開關。也可用選單鍵或
  Shift+F10 開啟。方向鍵和 Home／End 選擇項目，Enter 執行，
  Esc 關閉並返回原焦點。

### 9.1 觀看端設計：原生 DOM header（Chrome/Edge 形態）＋純頁面幀（第五期，LIU 2026-11-20 定并下令）

人通道的瀏覽器操作 UI（標籤條、導航、地址欄、菜單、尋找條）
是**觀看頁自身的原生 DOM**（`viewer.html`/`viewer.js`，R-17 拆分），
造型與呈現參照 Chrome/Edge：標籤條淺灰底、活動標籤圓角卡片與
導航行同底銜接、地址欄膠囊、32px 圓形圖標按鈕、卡片式下拉
菜單與右上浮動尋找條、加載進度線、深色跟隨系統配色。邊緣只
提供**狀態與執行**：經 WS 推送 `chrome-state`，收 `{type:
"chrome", action:…}` 命令執行——UI 的像素、焦點、選區、原生
輸入行為全部在觀看端本地，與真瀏覽器一致（地址欄打字零延遲、
右鍵粘貼是輸入框原生行為、Ctrl+F/Ctrl+L 是觀看頁快捷鍵）。

> 初版（2026-11-19）曾把工具欄由邊緣光柵化合成進視頻幀
> （chrome.rs 繪製 + fontdue），理由是防頁內注入污染 AI 快照。
> 第五期裁定改原生 DOM：注入污染的顧慮對**觀看頁**自身的 DOM
> 並不成立（它不在遠端頁面內、不進 AX 快照），而合成路的代價
> 是實打實的——地址欄每次擊鍵一個網絡往返、文字光柵化與逐幀
> 重繪、命中測試與頁面坐標糾纏。合成實現（Geo/Hit/Fonts/條帶
> 緩存）已整刪，`fontdue` 依賴退役。

**狀態協議**：邊緣 600ms 同步拍 + 事件即推，`chrome-state`
消息體：`tabs[{title,url}]`（不外洩 target_id）、`active`、
`canBack`、`canFwd`、`loading`、`find{text,info}|null`、
`res`（`"auto"|"WxH"`）、`gateAi`、`gateHuman`；逐字去重，
無變化不推；新連接建立即直發一份當前狀態。命令集：
back/forward/reload（加載中自動變停止）/navigate（邊緣
`normalize_input` 歸一化）/new-tab/switch-tab/close-tab/
ext/open-internal（限 chrome://）/print/find-open/find-close/
find-text/find-next/find-prev/res-auto/res-fixed/toggle-ai/
toggle-human（人通道只許在此關閉，重開走 /channels，與舊
口徑一致）。

**整頁幀需求**。WebSocket JPEG、RTC 視訊與 HLS 播放各自登記需求。第一個需求到達時，
邊緣先訂閱 CDP 事件，再開啟當前分頁的 screencast。最後一個需求釋放時停止採集並清空幀槽。
靜態頁缺少即時首幀時，邊緣補取 JPEG 並核對分頁、文件與視口後交付。
WebSocket 透傳原始 JPEG（8 字節頭 + JPEG）。只有編碼或貼圖需要 RGB 時才解碼，奇數邊裁至偶數。

**幀分類（2026-11-20 補）**：screencast 在視頻播放頁會
交替發「整頁幀」與「視頻區幀」（只有 video/canvas 元素
畫面、元數據報元素尺寸，實測約佔 1/3）。幀泵按尺寸分流：
寬高都達到整頁基準（見過的最大整頁尺寸）90% 的是整頁幀，
走透傳並把 RGB 歸一化到基準尺寸（整頁幀自身有幾像素的
高度變體）；更小的是內容幀，只供 RGB 貼圖，
解碼後按 video/canvas 的視口矩形貼回最近整頁 RGB（矩形
按內容尺寸查 CDP 並緩存，頁面滾動即失效），編碼層由此
只見恒定尺寸的整頁幀。舊實現一律當整頁幀，RTC/HLS 編碼
器跟隨尺寸反覆重建（生產一晚 468 次）、流分辨率來回跳、
觀看端視頻忽滿忽小（PITFALLS §39）。補充（同日晚）：
小於基準 90% 的幀也可能是視口縮小後的新整頁幀——分類
前以頁面當前視口（innerWidth/Height，500ms 緩存）對
照，尺寸恰等於視口即按新尺寸重定基準走整頁路徑，不
再誤作內容幀貼圖（PITFALLS §43）。

**幽靈指針改疊加層**：邊緣經 WS `ghost` 消息推位置/軌跡/
意圖標籤，觀看端按當前顯示面映射繪製（畫布/視頻按元素框
與幀尺寸比例、DOM 鏡像按布局+滾動），軌跡以池化小圓點
呈現；全模式一致（見 §7.1 實現口徑）。HLS 路不帶指針。

**輸入坐標**：WS mouse 坐標即頁面坐標（無工具欄偏移）；
懸停 mouseMoved 現在也轉發頁面（舊合成時代懸停只重繪工具
欄）。§7.1 的執行期遮蔽與接管提示不變：遮蔽是邊緣輸入路由
狀態、提示是觀看端 DOM。

**滑鼠事件**。按下、放開、拖動及滾輪使用同一條頁面輸入路徑。
觀看端傳遞座標、修飾鍵與點擊次數，邊緣逐連線記錄按住的按鈕。
DOM 模式會從同源 iframe 的局部坐標換算到主視口，並在重建後恢復頁面焦點。
左右鍵同時按住時可獨立放開，拖出舞台放開也會送達。單擊、雙擊、
中鍵 click（`auxclick`）和頁面右鍵事件由 Chromium 產生。
未傳點擊次數的 WS 事件按一次處理。

**WebRTC 主路**（Phase 1b，`rtc.rs`）：觀看端經同一條 WS 發
`rtc-start`，邊緣作 offerer + ICE-lite（str0m）回 `rtc-offer`，
觀看端以 `rtc-answer` / `rtc-candidate`（RTCIceCandidate 的
JSON 對象原樣透傳）完成协商。編碼全局一份：feeder 線程吃頁面
RGB（`rgb_seq` 變化檢測）以低延遲 H.264 編碼後廣播訪問單元，
多會話共享字節、各自 str0m 獨立封包；PLI 置共享強制關鍵幀
標誌，BWE 估計回寫編碼碼率（300k–8M 鉗制）。只有選用
RTC 視訊且 ICE 連通的連線停收 WS JPEG。RTC 資料通道及 Opus
音軌可在 DOM 模式獨立運作。觀看端用 `rtc-start` 或
`rtc-demand` 的 `video`、`audio` 宣告各軌需求，邊緣以
`rtc_video_want` 和 `rtc_audio_want` 記錄需求數。
host 候選檔的 UDP 套接字**必須綁在通告的候選地址上**——綁
通配地址時 str0m 以包 destination（0.0.0.0）與候選（具體 IP）
匹配不上，STUN 請求被靜默丟棄、ICE 永不連通（實測教訓，
真瀏覽器探針抓到並驗證修復）。

**幀率口徑（2026-11-20 修訂）**：feeder 節拍 `RTC_FPS=30`
（曾為 15，運動畫面被鉗在 ~14fps 是「視頻模式卡頓」的主因
之一）；幀泵（human.rs）按分段計時逐項削過——解碼 ffmpeg
~4.6ms（第五期起 metrics 的 compose 槽位記此段）；幀泵跟
不上時排水到最新幀（舊幀只有延遲沒有信息量）。第五期起 WS
路不再編碼（透傳原始 JPEG），編碼只剩 RTC/HLS 的進程內
管線。分段計時工具：`cargo test --release
bench_frame_stages -- --ignored --nocapture`（解碼與兩種編碼
對照）。實測運動源（60Hz rAF 動畫頁 / 60fps 視頻）觀看端
解碼 30.3–30.4fps、0 丟幀。教訓全錄見 PITFALLS §27。

**RTC 音頻軌**（Opus）：會話 offer 另帶一條 send-only 音頻
m-line。首個音訊需求啟動音頻 feeder，最後一個離開後關閉採集。從 rigsink 的
monitor 采集（與 HLS 的 AAC 管線共用 `open_pulse`，兩條管線
可並行掛同一 monitor）、解碼重採樣至 48k 立體聲後以 libopus
編碼（20ms 一幀、96kbps）廣播扇出，各會話按自己的 writer
寫入。音視頻共用同一紀元（RtcHub 建立時刻）：視頻時間戳
取頁面幀的解碼完成時刻、音頻以本輪采集首幀到達時刻錨定後按
採樣數累加，兩軌對同一時鐘打戳，觀看端按 RTP 時間戳
對齊（此前兩軌各自從零計數，起始抖動差可達 100ms+；
現殘差為兩側管線延遲差，約幾十 ms 級）。實測教訓三條：
①libopus 只收**打包**採樣格式
（flt/s16），平面格式開編碼器直接 EINVAL——與原生 AAC 收
FLTP 不同；②打包格式的交錯數據不能走 ffmpeg-next 的
`plane::<f32>`（按 samples 單聲道計長，讀會截半、寫端
copy_from_slice 長度不符直接 panic 殺死 feeder 線程），必須
走 `data(0)` 字節級讀寫並嚴格取 samples×聲道數；③feeder
線程以 catch_unwind 兜底，panic 按出錯重試、不帶走整個音頻
功能。採集前提：被採音的瀏覽器音頻須落在 rigsink 上——邊緣
自啟的瀏覽器由邊緣設 `PULSE_SINK=rigsink`；**掛靠模式下
外部瀏覽器須由啟動方自行設 `PULSE_SINK=rigsink`**（或系統
默認 sink 恰為 rigsink），否則 monitor 只有靜音。真觀看頁
探針驗證：offer 含 m=audio/opus、440Hz 音調穿 RTC 到觀看端
RMS 0.78（非靜音）。

**P2P 直連檔**：`--stun <host:port>`（可重複）配置後，會話
改走全 ICE（非 lite），且開始前以自寫的最小 STUN 客戶端
（RFC 5389 Binding、解 XOR-MAPPED-ADDRESS）從會話自己的
UDP 套接字采集 srflx 候選——str0m/is 按其設計不做 STUN
發現（「caller's responsibility」），故自寫（約百行，含單
元測試）。實測：經 NAT 向公網 STUN 采集到真實映射地址，
offer 同時攜帶 host 與 srflx 候選；全 ICE 與真瀏覽器連通
及 transport 門控均由探針驗證。經 Gateway 的涓流信令與
「雙端各連 Gateway」中繼檔屬軌道 T（Gateway 本體未建），
本模塊的信令消息形態與之兼容，待軌道 T 立項後接入。

觀看頁以 WebRTC 優先呈現視訊。ICE 斷開時立即回到 WS JPEG，
並留 3 秒給 ICE 恢復。8 秒內未建立 RTC 連線便關閉該會話。
影音開啟且 RTC 不可用時，影片模式可啟用 HLS 合流兜底。HLS
實際開始播放後才停收 JPEG。

**實現筆記**（第五期，`chrome.rs` / `human.rs` / 觀看端）：

- `chrome.rs` 只剩狀態：TabView/FindState/ResMode/ChromeState
  ＋ `state_json()` ＋ `normalize_input`（無協議頭按域名補
  https、其餘走 Google 搜索）。PointerVis 隨幽靈推送移入
  exec.rs。
- 起始 URL 直達初始標籤：自拉 Chromium 時起始 URL 作 argv
  末參（不再先 about:blank 再 navigate）——歷史只有一條、
  起始頁後退禁用，與真 Chrome 一致；掛靠模式仍 navigate
  過去（PITFALLS §31）。
- 視口釘死：邊緣拉起 Chromium 後以
  `Emulation.setDeviceMetricsOverride` 把視口釘在
  `--width/--height` 上——新版 headless 的 `--window-size`
  只定窗口、視口被虛擬屏裁剪（實測請求 900×700 只得
  900×500）。掛靠模式不釘，用瀏覽器自己的尺寸。
- AI 跟隨活動標籤：切換/新建/關閉標籤時 `set_main` 同步主
  會話；快照以會話 parent 鏈圈定當前頁面的幀樹
  （`frame_scope`），其他標籤的會話不混入觀察。
- 幀泵先訂閱 CDP 事件再開投屏：靜態頁面的首幀若在訂閱前
  到達會無人 ack、投屏停擺（實測中招後改的順序）。
- 列印：邊緣側 printToPDF 後經 `GET /print.pdf` 下載，觀看端
  收到 print-ready 通知後自動觸發。
- 觀看端 header 細節：活動標籤加載中 favicon 位變 spinner；
  地址欄非聚焦時才被狀態推送同步（不打斷打字）、https 顯
  鎖圖標/http 顯地球；尋找條 input 150ms 防抖發 find-text、
  Enter 下一個/Shift+Enter 上一個；主菜單項與舊合成菜單
  一一對應（尋找/新標籤/下載/擴展/歷史/設定/列印/解析度
  三檔帶勾/AI 通道/人通道帶勾）。

### 9.2 DOM 鏡像通道（Phase 2 原型，LIU 2026-11-19 立項；已實現）

Spike 數據（`docs/DOM_SPIKE_2026-11-19.md`）支持人通道改
混合分層：普通頁面走 DOM 鏡像、頁內媒體區域走視頻子流、
異常時整頁降級視頻。實現落在 `src/dom.rs`（邊緣通道 +
頁內序列化器脚本）與觀看端鏡像渲染器（`main.rs` 的
VIEWER_HTML）。架構口徑（LIU 2026-11-19 19:55）：內容面
DOM 樹流與視頻/音頻/圖片流並列一等公民、ffmpeg 全頁流
為兜底；傳輸面 WebRTC 優先、WS 回落。兩個後續增量均已
落地：DOM over RTC DataChannel（會話 offer 聲明 `rig-dom`
可靠有序通道，開通後快照/差分走 DC、斷開自動回落 WS，
控制消息仍走 WS；seq/重同步協議與傳輸無關、切換無縫，
雙端探針驗證）與 RTC 音頻軌（Opus，見 §9.1）。DC 大消息
分片（2026-11-20 補）：瀏覽器 SCTP 單條消息上限（Chrome
256KB）遠小於真實頁面快照（中文維基實測 1.6MB），整條直
發會被對端靜默丟棄——發送側對超限消息做應用層分片
（`dom-chunk` 幀，16KB/片、UTF-8 邊界切分），觀看端重組
後再走原處理；且 str0m 的 `Channel::write` 在發送窗不足
時以 Ok(false) 拒絕且不排隊，分片須先入會話待發隊列、
每輪循環末尾排空重试（bufferedAmountLow 閾值作喚醒）。

形態：邊緣經 CDP 向當前觀看會話注入頁內序列化器
（`Runtime.addBinding` 回傳差分、`Runtime.evaluate` 取
快照）。序列化器為 rrweb 路線：節點（含文本節點）分配
穩定 id，快照為 JSON 節點樹 + 樣式表文本（跨域樣式表退
為 `<link>` 引用）；`<canvas>` 序列化為帶幾何的佔位塊；
表單值/勾選/選中隨節點走。差分來自 MutationObserver
（childList/attributes/characterData），表單輸入與滾動
走旁路事件（MutationObserver 盲區，spike 實測確認）；
`<head>` 內突變直接觸發重同步（樣式結構變化不做增量）。

線協議（WS 文本，與 rtc-* 同層）：
- 觀看端 → 邊緣：`dom-start` / `dom-stop` / `dom-resync`；
  DOM 模式下輸入消息帶 `dom:1`，坐標為頁面坐標。導航/標籤/
  尋找等頭部操作走 §9.1 的 `chrome` 命令（兩種模式同一路），
  舊 `nav` 消息已刪。
- **頭部（第五期起）**：DOM 模式與視頻模式共用觀看頁頂部
  同一個原生 header（見 §9.1），鏡像填滿其下舞台區、視口
  不再扣條帶高。此前的過渡形態（邊緣合成幀裁條帶疊鏡像、
  `#stripCv`）隨第五期整刪。
- **輸入覆蓋層教訓（2026-11-20）**：視頻模式的透明輸入覆蓋
  層（`#ime`）恒覆蓋全屏，DOM 模式下把鏡像的真實指針事件
  全部截走，且其坐標按隱藏畫布換算得 null、落到邊緣 (0,0)
  被丟棄——鏡像自有轉發處理器從未收到真實事件（滾輪不滾、
  滾動條不出皆此根因；集成測試以文檔內直發事件繞過命中
  測試而未暴露）。修復：DOM 模式下覆蓋層
  `pointer-events:none`，事件由鏡像文檔自身承接。
- 邊緣 → 觀看端：`dom-snapshot {seq,url,w,h,css,links,
  tree}`、`dom-delta {seq,ops}`、`dom-state {state}`。
  seq 由邊緣單調分配；觀看端發現 seq 缺口即發
  `dom-resync`（DOM 版關鍵幀請求）。頁面導航後頁內脚本
  自動回 `installed`，邊緣據此重發快照。

觀看端：鏡像渲染在沙箱 iframe（`allow-same-origin`、
禁脚本/表單/導航），父頁按 id 映射應用差分；輸入在
iframe 內監聽後按頁面坐標轉發。鏡像須在不帶脚本下
成立——頁面自身的 JS 行為（動畫、輪播）不在鏡像跑，
靠差分流跟隨源頁面狀態。

**布局視口對齊（2026-11-20 補）**：iframe 的布局視口
釘為快照的遠端視口（w/h），再整體等比縮放（letterbox）
進舞台——舊做法鏡像填滿舞台、按舞台 CSS 寬度排版，觀看
端 DPR≠1 時遠端視口=舞台×DPR，兩邊布局（響應式斷點、
序列化凍結的固定像素尺寸）系統性錯位，點進度條恆左偏
100+px（LIU 實測）。對齊後鏡像與遠端逐像素同構：iframe
內事件 clientX 即遠端布局坐標、輸入轉發口徑不變；幽靈
指針與右鍵菜單的視覺換算按縮放比補正（PITFALLS §41）。
鏡像 head 另注入 `<meta name="referrer" content=
"no-referrer">`：子資源的 Referer 是觀看頁源，圖片 CDN
按外來 Referer 防盜鏈（Bilibili hdslb 縮略圖實測全滅），
no-referrer 實測放行（PITFALLS §40）。

**渲染模式對齊（2026-11-20 修訂）**：快照帶遠端
`document.compatMode`；鏡像 iframe 出身 about:blank 恒為
怪異模式，與標準模式頁面的流式布局系統性漂移（實測點擊
落點偏上 14px、遠端焦點落空、粘貼失效——小目標必偏、大
文本框偶爾蒙對）。模式不一致時觀看端以 srcdoc 重載鏡像
文檔對齊（快照暫存、load 後應用）。另補**鏡像焦點恢復**：
快照換 documentElement、差分刪節點（框架重渲染換節點）
都會把鏡像焦點打回 body，觀看端記住焦點控件的 rig id +
HTML id 簽名 + 輸入選區，快照/差分應用後按 id（或同 id
屬性的替身節點）恢復焦點與選區——只動觀看端本地焦點。
定因與驗證見 PITFALLS §28。

降級與邊界：DOM 模式開啟期間該連接停發 WS 二進制幀
（同 RTC 的 transport 門控）；快照失敗/超時、差分缺口
重同步後仍不收斂 → 觀看端回視頻檔。第一驗收點：OSM
式 JS 覆蓋層在活同步下的保真度（spike 靜態鏡像殘差
19.4%）。

canvas 子流（快照式，已實現）：快照里 canvas 只有佔位
塊；頁內序列化器以 1s 節拍對同源 2D canvas 做
toDataURL 捕獲、只發變化幀（`dom-canvas {i, d}`，帶外
子流無 seq，快照重建時強制重發）。空白幀（與同尺寸新
建 canvas 逐字相同）不發——WebGL 無 preserveDrawingBuffer
時捕獲恒為空白、自然落入此路，占位塊保留；跨源污染的
canvas 捕獲拋錯後不再重試；單邊 >1024px 或單幀 data
URL 超過約 1.5MB 的 canvas 不捕獲。觀看端收到後把占位
塊換成圖片、後續幀只換 src。驗證：集成測試斷言協議幀
（節點 id 對應、PNG data URL）；真觀看頁探針斷言鏡像
像素與源圖一致且重繪跟隨。

同源 iframe（已實現）：序列化器把同源 iframe 的
contentDocument 整棵放進節點的 `f` 字段（跨源讀不到、
只有佔位）；每個同源內嵌文檔挂 MutationObserver，內部
突變（1.5s 節流）與 iframe 導航都觸發整頁重同步，表單
值走共享 id 註冊表的 val 旁路；嵌套深度上限 4。觀看端
建節點時不帶 src/srcdoc（不許鏡像真去加載源頁面），把
`f` 樹建成該 iframe 自身文檔的根。邊界：DOM 模式的
輸入轉發不進 iframe 內部（事件不跨文檔），iframe 內
滾動不同步；視頻模式不受影響。集成測試在 DOM 通道
用例中斷言：內嵌頁表單經 f 進快照、內嵌頁自發突變
觸發重同步且新快照帶上新節點。

跨源 iframe 與 video 區域子流（2026-11-20 補）：跨源
iframe 序列化器讀不到內容（同源策略），節點帶 x 標誌、
rect 與 display；video 節點同樣帶 rect/display。邊緣泵
每 300ms 向頁內 `__rigDom.regions()` 取目標清單，按 rect
對頁面表面截圖（`Page.captureScreenshot` clip +
fromSurface，瀏覽器級合成面、不受同源策略限）廣播
`{"type":"dom-region", i, d}`：跨源 iframe 每 ~1.2s 或
rect 變化刷一幀、video 播放中每 ~300ms、暫停僅簽名
（rect+currentTime）變化時刷。觀看端為這兩類節點建佔位
div（display 保持原性質，inline 映射 inline-block——
普通 div 的 inline 不認寬高會塌 0×0），區域幀貼入並進
regionCache 跨快照保留。坐標型 CDP 輸入可穿透跨源
iframe（瀏覽器合成層路由，探針實測按鈕計數遞增）。

資源 URL 絕對化（2026-11-20 補）：鏡像文檔原點是邊緣
源，序列化保留的相對 URL 會按邊緣源解析（實測 YouTube
播放器 CSS 被向邊緣源請求回 401）。序列化器對 href/
src/poster/action/data/background 屬性、srcset、STYLE
文本與樣式表內 url()（以表自身 href 為基準）、@import
內聯內容（以 import href 為基準）、屬性差分一律按頁面
URL 絕對化；觀看端 applySnapshot 在新樹未連接時先放
`<base href=頁面URL>`+links+style 再換入。增量節流
30ms（原 100ms 主觀卡頓）。

增量存活三修（2026-11-20 補，定因見 PITFALLS §19）：
每次 dom-start 一律 setup+快照（頁面導航後序列化器是
新實例，只靠首入那次 start 增量會靜默死亡）；pump 的
installed/resync 先 setup 再快照；WS 斷線補 viewer_stop
且計數鉗下限；觀看端 pollAv 在 DOM 模式不再搶占退出。

觀看端依 `/channels` 的 `av_wanted` 訂閱 RTC 音訊。RTC 音軌
使用獨立音訊元素，DOM 模式仍可播放背景分頁的聲音。HLS 合流
由各觀看頁在影片模式啟停，模式切換與回退期間都保留使用者的
影音設定。

分辨率跟隨由舞台尺寸觸發 `resize`。首報立即發出，後續變更
等待 900 ms 安定。邊緣透過同一 `request` 的 `viewport`
回覆實際 CSS 視口，包含固定解析度或尺寸上限。觀看頁以此換算
畫布和視訊上的輸入及幽靈指針位置，DOM 快照也帶有實際視口。

幽靈指針（DOM 模式）：視頻模式下指針由邊緣合成進幀，
DOM 模式無幀可合成——WS 發送側以 150ms 節拍輪詢執行
狀態，位置/意圖標籤變化時下發 `{"type":"ghost", x, y,
label, pulse}`（頁面坐標），消失時下發 `ghost-off`；觀看
端在鏡像 iframe 上層用 div 疊加（按 iframe 視口偏移與
鏡像當前滾動換算）。集成測試在 DOM 通道用例中斷言：
HTTP act 點擊後 ghost 到達且落點在目標中心 40px 內。

實施筆記（2026-11-19 驗收實測）：
- 驗收結果：OSM 歡迎面板/Leaflet 控制層在鏡像中在位且
  幾何對齊（側欄源 [0,55,350,430] vs 鏡像 [0,55,350,444]，
  其餘錨點零偏差）；活同步像素差基線 7.2%、關面板後
  4.7%、拖動地圖後 4.5%（靜態 spike 為 19.4%）；拖動
  跟隨量 39–42%。表單雙向打字、節點增刪差分、窗口滾動
  同步（1200px 精確）均由雙端探針驗過。
- 序列化器三條實測教訓（都在 `collectCss`/`serRules`）：
  ①樣式表自身的 media 必須保留包裹——OSM 有一份
  `media="print"` 的表（20 條規則含 `#sidebar{display:
  none}`），丟了 media 打印規則在鏡像裡無條件生效、
  側欄頁眉全滅；②`@media (scripting:…)` 要改寫——鏡像
  iframe 禁脚本、scripting 求值為 none，頁面自帶的無-JS
  降級分支會誤觸發：純 scripting 門控的 none 分支丟棄、
  enabled 分支去條件展開，混有其他特徵的原樣保留；
  ③`@import` 同源可讀時遞歸內聯展開（跨源保留原行），
  否則被引規則的條件在鏡像端不可控。
- 視口變化（觀看端 resize 跟隨改了源頁面尺寸）會整體
  重排版：頁內脚本監聽 resize、防抖 350ms 後回
  `resync` 請邊緣重發快照，鏡像幾何以快照為準。
- **自動模式**每秒向當前分頁請求一次 `mode-summary`，回傳
  可見影片、區域及畫布面積比例與互動元素數（最多計至 3）。
  觀看頁維持既有的組合判據與 3 秒遲滯，手動切換會鎖定到下次
  導航或換分頁。只有實際顯示 DOM 的連線訂閱鏡像、差分及區域
  畫面。換頁後停止原分頁的觀察者。最後一個 DOM 訂閱離開時，
  同時回收差分和區域任務。判斷值可由 `window.__rigAuto` 查看。

## 10. 快照與動作語義（摘要）

- 快照為**全量緊湊快照**：一次 observe 回整頁 AX 樹的緊湊渲染 + 結構化
  Observation；元素 id 在邊緣生命週期內**穩定且永不復用**。
- 元素幾何批量取得（整改第四期 R-16）：每會話一次
  `DOMSnapshot.captureSnapshot` 建 backend→bbox 表（bounds
  為文檔坐標，減 getLayoutMetrics 的 pageX/pageY 得視口
  坐標），未覆蓋節點（無布局盒者，quads 本多為 None）回落
  逐節點 quads。實測 observe 743→294ms、CDP 調用
  1100→15 次（zh.wikipedia，明細經 /health 的
  last_snapshot 可查）。
- 動作必須攜帶觀察時的幀號；幀已推進 → 回 `StaleFrame` 拒絕執行（陳舊校驗）。
  因此調用方在引用元素前應以最近一次 observe 的幀為準。
- 動作前複核與拒絕回包（2026-11-21）：含元素目標且請求幀即最新存儲幀時，先做**單元素快速複核**
  （只取該元素的 AX 節點/DOM 屬性/位置，與整頁構建共用 `snapshot::element_core`），一致則沿用存儲幀、
  省掉整頁重採；不一致/出錯/不適用（換文檔、目標在子幀）一律退回整頁快照裁決，快路徑自己從不判拒絕。
  頁面狀態類拒絕會附狀態或採集失敗說明。本次動作的校驗幀須仍有效並在 500 ms 內才可複用。
  詳見感知層設計 §6.1。 [PERCEPTION.md](PERCEPTION.md)
- `Done.frame_id` 表示動作的校驗幀。需要更新時，`observe(since_frame)` 使用客戶端最後套用的觀察幀。
  每次請求都採集候選，靜止頁回空增量並推進幀號。分頁、文件、座標空間或投影不相容時回整幀。
- 密碼框整棵子樹脫敏（掩碼字符會經子文本節點洩漏，故按子樹清除，不做值替換）。
  DOM 映射失敗時，主幀回觀察錯誤，子幀略過並附診斷。
- Navigate 受 origin 白名單約束（`--url` 的 origin + `--allow-origin` 追加）。Chromium 回報導航錯誤時，動作回拒絕原因。
- Select 在改值並發送事件前驗證原生選擇框與所有指定值。未知或停用的選項會被拒絕。
- `tree_image` 與獨立截圖共用採集、驗證及遮罩流程。編號取自回應投影，遮罩涵蓋完整觀察與 DOM 中識別的敏感欄位。
  截圖前後核對文件、視口和敏感欄位的邊框。跨行程子幀含敏感欄位時遮罩其宿主框。無法確認時，
  `tree_image` 保留文字與樹並附診斷，獨立截圖回錯誤。

### 10.1 Upload 動作（協議擴展，LIU 2026-11-19 批准；已實現）

**動機**：`<input type="file">` 是 AI 操作真實網站的高頻卡點（上傳附件/
頭像/證件）。剪貼板路線拿不到 OS 文件；正道是給 AI 通道加一個 Upload
動作，邊緣經 CDP `DOM.setFileInputFiles` 直接給文件輸入框餵文件。

**協議**：rig-proto 新增 `Action::Upload { target, files: Vec<String> }`，
與其他動作同走幀號陳舊校驗與執行期語義。`files` 是**邊緣主機上、上傳根目錄內的路徑**：
根目錄為 `<cache-dir>/uploads/`；未給 `--cache-dir` 時用本進程的一次性臨時目錄 `<tmp>/rig-edge-<pid>-uploads`（LIU 2026-11-22；實際路徑啟動時打印到 stderr）；相對路徑按該根解析，絕對路徑須落在其內。
路徑先 `canonicalize` 再判斷，`..` 穿越與符號鏈接逃逸一律以 `policy_denied` 拒絕，交給 Chromium 的是規範化後的路徑。
人要給 AI 傳文件，把文件放進該目錄即可。

**校驗與錯誤**：目標元素須為 `<input type="file">`（否則拒絕
`not_file_input`）；每個路徑須在上傳根目錄內（否則 `policy_denied`）、存在且為普通文件（`file_not_found`）；
文件數 ≤ 20、單文件 ≤ 200 MiB（`file_too_large`）。三者是
RejectReason 的正式代碼（非 PolicyDenied 文案）。`multiple` 屬性
缺失且給多文件時只取第一個，並在 `ActionResult::Done` 的可選
`note` 字段中註明（該字段為本動作配套加入協議，無附註時不出現
在報文里、舊客戶端可正常解析）。

**實現**：ai.rs 定位元素拿 backendNodeId → CDP `DOM.setFileInputFiles
{backendNodeId, files}`；頁面隨後收到 change 事件，站點自身的上傳邏輯
照常跑。**回滾/遷移**：純新增動作種類，舊客戶端不發即無影響；協議
JSON 加字段向後兼容。

**人通道**：不做文件粘貼——觀看端瀏覽器讀不到觀看者本機文件給遠端
頁面，OS 文件選擇器屬於邊緣主機的瀏覽器。人要傳文件給頁面，用
AI 通道的 Upload 或站點自身的其他入口。

**測試**：集成測試 `edge_upload_action` 覆蓋正例（頁面 change
收到文件名:大小）、負例（`not_file_input`/`file_not_found`/目錄外 `policy_denied` 按碼
斷言）與單選截斷（Done.note 註明、頁面只收到第一個文件）；
rig-proto 單元測試鎖定 upload/新拒絕碼/Done.note 的線格式。

## 11. 已知限制與未做事項

- **遠端接入不在本倉庫演進**：網關/傳輸層（Gateway/Connector/CLI/中轉）已整倉移除
  （另有備份）；邊緣不依賴轉發中心即可獨立作控制端使用（見 §1）。
- **人通道呈現是 CDP screencast（JPEG 幀流）+ WebRTC + HLS**，不是 RDP；
  RDP 呈現（IronRDP）與 RDP 音頻是後續里程碑。
- **人通道文件粘貼不做**：觀看端讀不到使用者本機文件；AI 通道的文件
  需求由 Upload 動作承擔（§10.1）。
- **增量觀察的基幀池只留最近 8 幀**：基幀更舊的 since_frame
  回完整視圖（協議即如此設計，客戶端重新定基即可）。
- **DOM 模式邊界**：頭部已與視頻模式一致（邊緣 chrome 條帶流，
  含標籤條與下拉菜單，見 §9.2）；同源 iframe 內容整棵鏡像；
  跨源 iframe 與 video 走**區域子流**（邊緣按 rect 截圖貼回
  佔位，見 §9.2）——驗證碼部件可看可點（坐標型 CDP 輸入由
  瀏覽器合成層路由、可穿透跨源 iframe，探針實測），但 iframe
  內部滾動不同步、內部 DOM 不鏡像；canvas 子流限制見 §9.2
  （單邊 >1024px、單幀 >1.5MB、WebGL 空白、跨源污染不捕獲）；
  頁面自身 JS 行為不在鏡像執行。內聯 SVG 按命名空間重建
  （2026-11-20 修，PITFALLS §34），圖標類元素可正常顯示。
- **YouTube 等站點的機器人門檻（站點側限制，非邊緣缺陷）**：
  數據中心出口 IP + 自動化瀏覽器會被 YouTube 判為機器人，播放器
  位置直接換成「登入以確認你不是機器人」提示、媒體不加載
  （video readyState 0、無 src），視頻與 DOM 兩模式同樣播不了
  （2026-11-20 CDP 直讀實頁定因，PITFALLS §34 旁證）。出路在
  部署/帳號層：觀看端登入 Google 帳號（持久化雙目錄已落地，
  見 §4——登入一次跨重啟保留）、或換住宅出口；邊緣代碼無可修。
- **Bilibili 同屬站點側門檻**：視頻頁被直接重定向到日本版首頁
  錯誤路由（`/ja?spm_id_from=…selfDef.erro`），播放器根本不
  創建（2026-11-20 實測）。邊緣播放機件本身已全量驗清：
  H.264/AAC/AV1 的 canPlayType 與 MSE、Widevine 均可用，本地
  WebM/MP4 與公網 MP4 實播時間軸正常推進，DOM 模式視頻區域
  子流持續出幀——「播不了」先查站點對出口的處置，再疑邊緣。
- **瀏覽器內部頁（chrome:// 設置/下載/擴展等）僅視頻模式**：
  WebUI 界面全在 shadow DOM 裡，序列化器只能拿到空殼樹、鏡像
  顯示不出實內容。DOM 通道按 scheme 把關（chrome://、
  chrome-extension://、about:、devtools:// 等）：不產快照、
  直接報「內部頁面不支持 DOM 鏡像」狀態，觀看端回落視頻模式
  並給出提示（screencast 對 WebUI 正常出幀，實測）。
- **右鍵菜單為觀看端自繪**：原生上下文菜單是瀏覽器 UI 層、不在
  截幀內（探針實測），自繪是唯一路。現行實現畫在觀看端：菜單項
  是橋接動作（複製選區/貼上/全選/後退/重新載入 + 異步追加的
  「複製圖片」），右鍵本身照常轉發頁面——頁面自帶 contextmenu
  邏輯不再被污染（上一版頁內注入菜單的取捨已消除）。
- **剪貼板在非安全上下文降級**：觀看頁經局域網 HTTP 打開時
  Async Clipboard API 不可用——文字複製降級為手動複製框（選區
  文本已備好，Ctrl+C 即走），圖片回拷不可用；正道是開 HTTPS
  （`--tls-cert/--tls-key`，LIU 2026-11-20 定版路徑），安全
  上下文下全走標準 API（實測通過）。
- **RTC 唇音同步殘差**：已按共享紀元對齊（見 §9.1），殘差為
  兩側管線延遲差（約幾十 ms），未做採樣級實測校準。
- **人/AI 幀無統一編號（R-15 輕量處置）**：人通道視頻幀與 AI
  Observation 幀是兩條管線，沒有共同幀號；關聯性由 observe
  採集前後的主文檔 loaderId 代次校驗保證（採集期間導航會在
  觀察文本裡警告，見 R-14）。完整版跨通道幀號方案曾存檔於已移除的
  评审文档，LIU 2026-11-20 指示：出現跨通道對證/新鮮度/時間對齊類
  問題時提醒他，可重寫一版實測。
- **多標籤的 AI 對象（R-24 口徑澄清）**：AI 通道只觀察/操作
  人當前活動標籤的頁面樹（快照按主會話圈定範圍）；用戶切換
  標籤後，舊幀的元素校驗必然拒絕後續動作，屬預期行為——
  AI 應重新 observe。`docs/SECURITY_DESIGN_2026-11-19.md`
  B1 的早期設想（觀察剝離非活動標籤）已被現行實現取代：
  非活動標籤根本不進快照。
- 桌面版邊緣程序（UIA/AT-SPI2/AX）未開工，排在 Android（P5）之後；
  純像素理解為 P6 兜底，未立項。
- CI 未設瀏覽器 E2E job（GPT review R-13）：集成測試目前靠提交前
  本地 local-gate 執行。
- R02（DB 跨租戶約束）經 LIU 2026-11-19 裁定 **de-scope**：RIG 是
  個人/受信環境、人工監督的工具，不做多租戶隔離（评审记录随网关
  文档一并归档，不在本仓库演进）。
- **踩坑手冊**：本子項目兩晚踩過的坑（症狀/根因/解法/驗證）統一
  收在 `docs/PITFALLS.md`（LIU 2026-11-20 定版要求），新坑先落
  該冊再回頭改本節。

## 12. 集成測試

`crates/rig-edge-browser/tests/edge_it.rs`，以環境變數 `RIG_EDGE_IT=1` 啟用
（需真 Chromium；`RIG_CHROMIUM` 可指定路徑，否則掃 Playwright 安裝目錄）：

| 測試 | 覆蓋 |
|---|---|
| `edge_browser_end_to_end` | 登入流程全鏈：observe → 填表 → 點擊 → 歡迎語斷言；iframe 內表單；陳舊幀拒絕 |
| `edge_text_keys_clipboard` | 文本鍵入、組合鍵（Ctrl+A/C/V 跨框複製）、IME 整段送入 |
| `edge_av_and_image_clipboard` | HLS 雙版本播放列表與分片、440Hz 音調非靜音驗證、圖片剪貼板雙向（尺寸驗證） |
| `edge_token_auth` | 無令牌拒絕啟動、令牌只認 Bearer（URL 令牌 401）、換票、access 不得調 AI 路由、refresh 輪換與舊票重放 401、靜態殼免令牌 |
| `edge_stdio_mcp_and_attach` | MCP：initialize/tools/list/observe/act/screenshot 全流程 + stdin EOF 正常退出；掛靠：`--cdp-url` 連現成 Chromium、邊緣退出後瀏覽器存活 |
| `edge_chrome_ui_and_takeover` | chrome UI：地址欄導航、後退、菜單尋找條、標籤新建/關閉；Point 越界與陳舊幀拒絕；執行期遮蔽 + 強制接管（在途 act 回 Aborted{UserTakeover}、頁面動作未落地） |
| `edge_dom_channel` | DOM 通道全鏈：快照建成、val/add/scr 差分、seq 嚴格遞增、resync 收斂；DOM 模式幽靈指針下發與落點；canvas 子流幀（節點 id 對應、PNG data URL）；同源 iframe 內容經 f 子樹進快照、內嵌突變觸發重同步 |
| `edge_profile_persistence` | 持久化雙目錄：Cookie / LocalStorage / IndexedDB 寫入 → SIGTERM 優雅停機（cookie 名與 ls 值落盤實據斷言）→ 同 `--profile-dir` / `--cache-dir` 重啟 → 三值全數恢復 |
| `edge_internal_gate_and_dblclick` | 雙擊透傳（視頻/DOM 兩路 count=2 → 遠端 dblclick 計數遞增，判據走 DOM 快照——observe 判據會按 §7.1 遮蔽人手勢自鎖）；內部頁把關：切 chrome://settings 後只收內部頁狀態、無該頁快照 |
| `edge_act_fast_precheck` | 動作前單元素快速複核：頁面沒變時主頁面元素全部命中快路徑（證明單元素算出的 role/name/states/bbox 與整頁構建一致）；子幀元素退回整頁；頁面自己變了（移位/禁用/消失/換文檔）仍必須被拒 |
| `edge_observe_unchanged_vs_changed` | 呈現一側：沒變 → 空增量且不採集；頁面自己變了/視口滾動（無人操作）→ 必須重新採集並帶出變化；點擊後 act 立即返回、按需取增量（不引起變化 → 空增量；改文字 → 增量帶出新文字） |
| `edge_dom_subscription_keeps_jpeg_frames` | 後台 DOM 感知訂閱不得停發 WS JPEG 幀；`jpeg-demand off` 後才停 |
| `edge_upload_action` | Upload 動作：正例（頁面 change 收到文件名:大小）、負例按碼斷言（not_file_input / file_not_found）、單選文件框多文件截斷（Done.note 註明、頁面只收首個） |
| `edge_observe_modes_budget` | observe 全字段：mode=text/tree/tree_image 響應組成、budget_tokens 裁剪響應視圖且存儲幀不動（裁剪後點擊仍成）、subtree 子樹與不存在 id 回 400、include_offscreen 默認過濾視口外元素、since_frame 增量（基幀對齊、upsert 含新值、未知基幀回完整、text 模式不走增量） |
| `edge_dom_sub_idempotent_and_cleanup` | D01/D02：同連接重複 dom-start 只訂閱一次；配對 stop 回零、多餘 stop 不倒扣；直接斷開（不發 dom-stop）名額照常回收（/health `dom_viewers` 斷言） |

測試側約定（實測教訓，勿退回）：

- 全部測試以 tokio Mutex **串行**執行——每項都拉 Chromium（AV 項另跑 x265
  編碼），在共享開發機上全並行會互相擠兌造成假失敗（clippy 不許 std 鎖跨
  await，故用 tokio 鎖）。
- 被測進程一律 `process_group(0)` 自成一組，EdgeProc Drop 時整組擊殺
  並按瀏覽器配置目錄名（`rig-edge-<pid>`）掃命令行兜底：只殺邊緣本體
  會漏掉它拉起的 Chromium 子樹，孤兒進程帶著投屏/編碼循環空轉，曾
  兩度把共享開發機吃滿到 sshd 失聯（2026-11-19）。
- 手工拉起的 Chromium 其 profile 目錄必須每次唯一：固定目錄會讓殘留實例
  接管、新進程直接退出，調試端口永不開放。
- 讀 Chromium DevTools HTTP 端點（如 `/json/version`）必須按
  Content-Length 讀滿正文——該端點保活連接，`read_to_end` 會等到超時。
- 測試頁元素的 AX 名稱來自 `<label>` 文本（如「用戶名」），勿憑假設寫
  期望名。

全倉提交門禁 `scripts/local-gate.sh` 包含 fmt、clippy、單元測試、真 Chromium 集成測試和 Python 報表測試。
執行前備妥完整 Chromium、PulseAudio 虛擬聲卡與 `ffprobe`，提交前須全綠。

## 13. 本批次提交索引

| 提交 | 內容 |
|---|---|
| `54c182c` | v1：AI 通道（AX 快照 + 陳舊校驗動作 + 編號截圖）+ 人通道（screencast + WS 觀看頁）+ 通道閘門 |
| `851a5b9` | AV 合流（虛擬聲卡 + ffmpeg，HEVC 主 + H.264 兜底）、圖片剪貼板雙向、輸入法與組合鍵修復（key_spec 根因） |
| `26dc971` | 直連鑑權：令牌中介層 + 非回環無令牌拒絕啟動 + 播放列表令牌改寫 |
| `4bf5d08` | MCP stdio 前端 + `--cdp-url` 掛靠模式 + 虛擬聲卡自愈 + 測試串行化 |
| `47dfd43` | GPT review A 批正確性修復（R-05 沙箱收緊、R-08 CDP 資源清理、R-09 令牌解碼、R-10 渲染轉義、R-11 調試脫敏、R-12 pulse 自有進程、R-14/15/16 動作保真、R-17 start 復位）；同提交附 B 批安全設計稿 `docs/SECURITY_DESIGN_2026-11-19.md`（待批） |
| `c9da3ea` | B4+交互模型協議側：rig-proto Aborted{UserTakeover} + ai.rs 執行狀態/中斷檢查點 |
| `8413591` | 人通道補強：邊緣合成 chrome UI、canvas 薄客戶端、AI 幽靈指針與遮蔽/接管、視口釘死、幀高湊偶 |
| `08827b3` | 集成測試：chrome UI/強制接管/Point 負例 |
| `8950f47` | docs：§7.1/§9.1 改「已實現」並補實現筆記 |
| `e342dd0` | Phase 0：WS 二進制幀（8 字節頭 + JPEG 原始字節） |
| `2a72ab7` | Phase 1a：進程內編碼層 encode.rs（libavcodec + hw_frames_ctx 運行時協商） |
| `8b01cd6` | Phase 1a：HLS/AV 合流遷進程內管線（libavformat + avdevice 音頻採集） |
| `4638bdd` | docs：§8/§9.1 進程內編碼管線與音頻實測教訓 |
| `c117cba` | Phase 1b：WebRTC 主路 rtc.rs（str0m offerer+ICE-lite、共享 feeder、PLI/BWE 回寫） |
| `1b17ee5` | Phase 1b：WS 信令與按連接 transport 模式 |
| `aa46fca` | docs：§9.1 Phase 1b 與 UDP 綁候選地址教訓 |
| `2f9c589` | Phase 1c：觀看端 WebRTC 接入與自適應回退 |
| `275a057` | docs：§9.1 Phase 1c 回退語義 |
| `97442b9` | P2P 直連檔：`--stun` 全 ICE + 自寫 STUN 客戶端 |
| `52a48f2` | docs：§9.1 P2P 實測與 Gateway 信令待軌道 T |
| `fb1d03e` | docs：DOM 鏡像 spike 報告 `docs/DOM_SPIKE_2026-11-19.md` |
| `83f11f7` | Phase 2：DOM 鏡像通道 dom.rs（頁內序列化器 + 邊緣轉發） |
| `2e882fd` | Phase 2：人通道 current_view + DOM 模式輸入直達 |
| `492c746` | Phase 2：DOM 模式接線與觀看端鏡像渲染器 |
| `303eae2` | docs：§9.2 改已實現（架構口徑、驗收數據、序列化器教訓） |
| `f619d3d` | docs：Phase 2 提交索引補入 |
| `c10d477` | docs：子項目定位定版（邊緣不依賴 Gateway 獨立可用、受控接入為擴展）入 §1/§1.1，ARCHITECTURE/ROADMAP 同步 |
| `806fdfd` | 集成測試：DOM 通道用例（快照/差分/seq/resync） |
| `180540c` | DOM over RTC DataChannel：rig-dom 可靠有序通道，開啟後快照/差分走 DC、關閉回落 WS |
| `f6454e5` | RTC 音頻軌（Opus）+ 觀看端 AV 優先級翻轉（RTC 優先、HLS 兜底） |
| `3e13975` | 集成測試進程守衛：整組擊殺 + 配置目錄清掃，杜絕孤兒進程 |
| `134a75c` | Upload 動作：rig-proto 協議擴展（Action::Upload、新拒絕碼、Done.note）+ CDP DOM.setFileInputFiles + 集成測試 |
| `de82e17` | R-18：/observe 與 MCP observe 落實 ObserveRequest（mode/budget/subtree） |
| `1e7853c` | DOM 模式補完：AI 幽靈指針 overlay（WS 下發頁面坐標）+ canvas 子流（快照式） |
| `7c108a3` | docs：§11 已知限制刷新、§12 測試表補全、§13 索引補本輪提交；review/STATUS.md 補 RIG-R02 de-scope 記錄 |
| `7610a30` | observe 補完：include_offscreen 落實（默認過濾視口外元素）+ since_frame 增量（rig-proto diff/apply 落地、8 幀基幀池、HTTP delta 字段/MCP 雙文本塊） |
| `a95064a` | DOM 鏡像補完：同源 iframe 內容經 f 子樹鏡像（內嵌突變走重同步）+ canvas 上限 640→1024/單幀 1.5MB |
| `c52ed6e` | RTC 唇音同步：音視頻共享紀元打戳（視頻按合成幀時刻、音頻首幀錨定） |
| `f0cb830` | §9.1 工具欄條帶緩存（狀態哈希鍵控單條緩存，菜單/指針不進） |
| （本提交） | docs：§11/§12/§9.1/§9.2 隨上述四項刷新（ObserveRequest 全字段落實、DOM iframe 邊界改寫、唇音殘差與條帶緩存條目更新） |
| `91a94f3` | RTC 觀看端黑屏修復：ontrack 改為把音視頻軌累加進同一 MediaStream（音頻軌後到曾把只含音頻的流蓋到 video 上） |
| `052ff54` | screencast JPEG 解碼修復：image/zune-jpeg 解 Chromium 真實幀系統性綠偏，改走 ffmpeg 解碼 + 真實幀 fixture 回歸測試 |
| `6fd1f7c` | 觀看端輸入整批修復：DOM 按鈕不再被 AV 靜默攔截、鼠標消息帶 button/buttons、剪貼板和弦放行 keydown 默認動作、地址欄編輯態退出與字符鍵入 |
| `ec86881` | 拖選修復：mouseMoved 同時帶 button=<按下鍵> 與 buttons 掩碼；觀看端 mousemove 節流加尾幀補發 |
| `a0fd18b` | DOM over DC 大消息分片：dom-chunk 應用層分片（16KB/幀）+ 觀看端重組 + 分片單測 |
| `23ef07e` | DC 分片背壓修復：分片入會話待發隊列、每輪排空重試；ChannelOpen 設 bufferedAmountLow 閾值 |
| `56dd0f7` | pollAv 競態修復：domAvOffPending 標記防進 DOM 時被自己的輪詢誤殺 |
| （本提交） | docs：§9.2 補 DC 分片/背壓口徑、§13 補 2026-11-20 凌晨實測修復諸提交 |
| `96d7e9c` | 觀看端整批修復（LIU 2026-11-20 復測反饋）：頁內自繪右鍵菜單（原生菜單不在截幀內，注入腳本 + `__rigMenu` binding 回傳動作，裸頁面有導航兜底項）、剪貼板同步兑現（邊緣 700ms 同步剪貼板+選區推送，copy 事件內以選區兑現，繞非安全上下文限制）、`#ime` 光標改 default、DOM 條帶頭部（chrome 合成幀裁條帶經 WS 疊加鏡像，dbar 退役；發送側 DOM 優先於 RTC 跳幀）、DOM 輸入根因修復（`#ime` 全屏覆蓋截走鏡像事件且坐標換算為 null——DOM 模式覆蓋層 pointer-events:none，鏡像補 paste/copy/cut）、序列化器 scroll 節流改尾隨推送（停穩位置不丟） |
| `adbad8f` | 觀看端第二輪修復（LIU 2026-11-20 再復測反饋）：DOM 資源 URL 全量絕對化 + `<base>` 建樹前插入（YouTube CSS 向邊緣源請求 401 根治）、剪貼板顯式模型（clipboard-get 請求/回應 + 手動兜底框，整刪 700ms 同步與暫存補寫；clipboard-read-image 恢復為顯式讀回；DOM 複製以鏡像選區為準）、右鍵菜單改觀看端繪製（橋接動作 + 按節點 id 取圖，拆頁內注入菜單）、跨源 iframe/video 區域子流（rect 截圖 dom-region；佔位 display 保真 inline→inline-block）、鏡像輸入 attach-once、增量節流 30ms、UA 規範化去 Headless（YouTube 實播通過）、TLS 自簽名服務（`--tls-cert/--tls-key`）、DOM 增量靜默死亡三修（viewers 斷線泄漏/導航後無人 start/pollAv 誤殺）；新建 `docs/PITFALLS.md` 踩坑手冊。驗證：local-gate 全綠、IT 9/9、probe-batch4 22/22×2、HTTPS 剪貼板直寫、YouTube 實播 |
| `acc5361` | 瀏覽器人格補完（LIU 定向：不保留無頭/CDP 開發形態痕跡）：UA 覆蓋連帶 userAgentMetadata（版本取 product 真實完整版）、acceptLanguage 經覆蓋下發（不帶 q 值）、時區覆蓋（默認 Asia/Tokyo）、去 `--disable-gpu` + `--enable-unsafe-swiftshader` 恢復 WebGL、device metrics 連帶 screen 尺寸；僅自拉瀏覽器生效、掛靠真實瀏覽器跳過。PITFALLS §25。驗證：指紋電池 dbg24 前後對照全項達標、local-gate 全綠、IT 9/9 |
| `31d7b47` | LIU 復測三修（2026-11-20）：地址欄右鍵（中/右鍵落地址欄例外進編輯態、DOM 條帶右鍵開菜單、菜單加「粘貼並轉到」；§9、PITFALLS §26）、視頻流暢度（RTC_FPS 15→30、BWE 上限 8M、幀泵排水到最新幀、JPEG 編碼改 ffmpeg mjpeg、合成快路徑、幀泵分段計時 bench；§9.1、PITFALLS §27）、DOM 保真（快照帶 compatMode + 鏡像模式對齊重載、鏡像焦點/選區恢復；§9.2、PITFALLS §28）。驗證：dbg27 雙模式右鍵粘貼導航、dbg28 動態重建頁粘貼落遠端、dbg29 運動源 30.3–30.4fps、dbg30 點擊落點定因、local-gate 全綠、IT 9/9 |
| `99bc6f8` | 整改第一期·控制閉環（GPT 審查 R-01/R-02/R-03，計劃見 `docs/REVIEW_REMEDIATION_2026-11-20.md`）：ExecControl 改取消代次 + 在途計數 + 接管闩鎖（observe 解鎖）；ai.rs 副作用注入逐步檢查代次（ExecErr 區分 Aborted/Rejected，press/release 與 key down/up 之間補釋放再報中斷）；exec 單測三項（保持窗/代次不復活/闩鎖）；IT 接管用例加闩鎖與恢復斷言。§7.1 實現筆記同步 |
| `4325bf4` | 整改第三期·性能調度（F-01/02/03/04/05/06 + R-20）：區域採集拆獨立任務（100ms 節拍）、播放中 video 區域 280→150ms；JPEG 按連接顯示模式記賬（jpeg-demand，僅 canvas 模式編碼、斷開兜底釋放）；合成/原始幀 Arc 共享（RTC feeder 零克隆）；條帶與畫布 mousemove 最新值合併（~24ms+尾點補發）；canvas 尺寸守衛 + 過期 bitmap 必 close；音頻隊列有界 64 滿丟包、HLS 讀文件鎖外 IO。同提交存檔 R-15 協議版為待啟用提案（見整改計劃「待啟用方案存檔」）。驗證：local-gate 全綠、IT 9/9、dbg31 探針（demand 關=0 幀、DOM 模式 region/delta 並行） |
| `6da9adc` | 整改第二期·隱私與穩定速修：TLS 強制（非回環未給證書自動自簽名 `~/.rig-edge/`、私鑰 0600 原子建立 R-22 後半；探針實測 HTTPS 通/HTTP 拒）、調試 URL 去 userinfo（R-22 前半）、WS 收包按幀型分發 Ping/Pong/Binary 不拆連（R-10）、人通道啟動失敗如實降級 + `human_error` 可查（R-11，啟動與 /channels 兩路）、序列化器 rm 清子樹映射 + 增量 3000 條上限轉 resync、觀看端 mirMap rm 清子樹（R-08）、request_id 冪等（鍵 = id + 請求指紋；同 id 不同內容照常執行；Aborted 不緩存；IT 加真重試斷言）（R-06）、快照提交單調性 + focus_session 用配對幀（R-05）、副作用 Point/Key 動作實時復核命中/焦點（R-04）、次級 frame 失敗警告到達觀察者 + 主 frame 失敗報錯（R-12）、CDP 總截止 20s（含排隊發送）+ 讀泵結束 fail 全部 pending + 在途上限 512（R-13）、observe 採集前後 loaderId 代次校驗警告（R-14）、§11 補 R-15/R-24 口徑。驗證：local-gate 全綠、IT 9/9（65.9s，含幂等新斷言）、TLS 探針實測 |
| `ef484d3` | 整改第四期·量化與自適應：metrics 分段計量底座 + /health 可觀測字段 + /ready 就緒探針（F-09/R-19）；R-16 先量測後批量化（captureSnapshot 一次建幾何表，observe 743→294ms、CDP 1100→15 次）；F-08 觀看端自動模式（進入時點裁決、組合判據、PITFALLS §29）；F-07 本地即時反饋（漣漪/拖選框/滾動預覽，結果態不偽造）。驗證：local-gate 全綠、IT 9/9（71.0s）、dbg35 雙場景自動命中 + 反饋可見、dbg36 滾動預覽確定性斷言 |
| （本提交） | docs：整改計劃第四期執行記錄、§5 /ready 與 health 字段、§9.2 自動模式/本地反饋、§10 批量幾何、PITFALLS §29 與 pkill 括號教訓 |
| `a67720f` | 整改第五期·header 拆分：觀看頁原生 DOM 工具欄（Chrome/Edge 形態，viewer.html/viewer.js include_str 嵌入 = R-17）+ 邊緣 chrome-state 推送/命令 + 純頁面幀（WS 透傳原始 JPEG）+ 幽靈指針全模式 overlay + chrome.rs 去合成（fontdue 退役）；起始 URL 直達初始標籤（PITFALLS §31）；last_frame 幀泵無條件記錄（PITFALLS §30）。驗證：local-gate 全綠、IT 9/9（50.2s）、dbg37 系列真觀看頁探針（導航/前後態/菜單/尋找/標籤增刪/雙模式共用頭部/幽靈落點/接管提示） |
| （本提交） | docs：整改計劃第五期執行記錄、§9.1 整節改寫（原生 header + chrome-state 協議 + 純頁面幀）、§5 加 /viewer.js、§2/§7.1/§9/§9.2 隨改、PITFALLS §30/§31 與 §六 pkill 同行自匹配補條 |
| （本提交） | LIU 第二輪實測缺陷修復：DOM 模式文字粘貼恢復本地回顯（遠端落字不變，PITFALLS §33）；鏡像 SVG 按命名空間重建、圖標恢復（PITFALLS §34）；§11 補 YouTube 機器人門檻為站點側限制。驗證：local-gate 全綠、IT 9/9（49.4s）、dbg39 粘貼鏡像/遠端雙斷言、dbg40 YouTube 鏡像截圖目檢（圖標全數渲染） |
| （本提交） | 持久化雙目錄（LIU 定版）：`--profile-dir` / `--cache-dir`（含環境變數回落）+ `--password-store=basic` 可遷移口徑 + SIGINT/SIGTERM 優雅停機（Browser.close 落盤，HTTP/stdio 同口徑）；§4 補參數與停機口徑、§12 補用例行。驗證：local-gate 全綠、IT 10/10（52.1s，含新 `edge_profile_persistence`：三類存儲寫入→TERM 停機落盤實據→重啟全恢復） |
| （本提交） | LIU 第三輪實測三項：內部頁 DOM 把關（dom.rs 按 scheme 不產快照、報狀態回落視頻，PITFALLS §37）；雙擊歸遠端（拆觀看頁自綁 dblclick 全屏、count 透傳進 CDP clickCount、缺省 1，PITFALLS §36）；視頻播放全量驗清為站點側門檻（編解碼/MSE/Widevine/實播/區域子流皆正常，YouTube 機器人牆與 Bilibili 錯誤路由跳轉，§11）。驗證：local-gate 全綠、IT 11/11（58.4s，含新 `edge_internal_gate_and_dblclick`），dbg45–49/53/54 探針（播放矩陣、區域幀、四變體點擊、§7.1 遮蔽歸因） |
| （本提交） | LIU 第四輪實測（Bilibili）三項：幀泵幀分類（screencast 視頻區幀貼回整頁合成、整頁 RGB 歸一化基準尺寸，RTC 編碼器重建 468→2 次/45s，PITFALLS §39）；鏡像 no-referrer（hdslb 縮略圖 3/35→31/36 載入，§40）；鏡像布局視口對齊遠端 + 等比縮放（進度條點擊偏差 −100~150px→+7px，§41）。驗證：local-gate 全綠、IT 11/11（59.0s），dbg56–63 探針（基線對照、幀尺寸普查、編碼器日誌、兩 DPR 點擊實測） |
| （本提交） | LIU 第五輪實測兩項：無聲定因在觀看端狀態機——domStart 關影音後退出不恢復 + vid 静音無收斂點（§42）；修復 avWasOn/restoreAvAfterDom 三路徑恢復 + showDisplay 静音收斂。視頻仍跳：reportSize 改 900ms 安定滯後 + 幀泵視口對照重定基準（§43）。音頻鏈全程探針洗清（源頭 audioDec、monitor 電平、RTC ~300B/包、觀看端渲染電平皆實測非零；途中兩度被自家探針夾具誤導，§44）。驗證：local-gate 全綠、IT 11/11（59.9s），dbg64–68 探針（DOM 往返後電平 0.21、尺寸橫跳 8 次僅 1 次安定重建） |
| （本提交） | LIU 第六輪：仍無聲——生產 /channels 實測 av:false 恒關（§42 的同會話恢復沒覆蓋「恢復代碼沒跑到」的情形：舊版緩存頁/DOM 期間關標籤；且觀看頁菜單無 AV 開關、用戶無從手動恢復，§45）。修復：pollAv 兜底自愈（視頻模式見 av 關且可用即開回，DOM 不搶）+ 觀看頁/腳本 no-store。視頻固定分辨率下偶爾小跳：本會話編碼器零重建（固定模式尺寸恒定），疑內容幀貼圖 rect/sf，幀泵加 rect 取樣診斷日誌待下輪取證。驗證：local-gate 全綠、IT 11/11（58.9s），dbg69 探針（外部強關 8s 內自愈、DOM 期間不搶、退出恢復） |
| `94abd21` | 獨立審查批次 1（控制正確性）：接管闩鎖單臨界區原子化（C01）+ 舊 observe 不得解闩（C02，需接管後新句柄）+ 在途計數改 Drop guard 取消安全（A02）+ Done 攜本次刷新幀號、刷新失敗如實註明需重 observe（A01）。驗證：local-gate 全綠、exec 6/6（含風暴/舊代次/abort 回歸） |
| `3d39ffb` | 獨立審查批次 2（連接生命週期）：DOM 訂閱同連接冪等守衛（D01，自動感知+前台共用一份）+ 釋放點收斂父作用域（D02，任一側先結束不丟清理）+ RTC DC 待發隊列有界（T01，4MB/512 幀/5s 駐留，超限整代次丟棄+請求重同步）。驗證：local-gate 全綠、新增 `edge_dom_sub_idempotent_and_cleanup` 真 WS 集成測試 |
| `04d95c3` | 獨立審查批次 3（視頻 P1）：內容幀改貼乾淨整頁底圖（V02，舊位置殘影根除）+ rect 緩存 400ms TTL（V01）+ 唯一歸屬匹配（V04，多 video 近尺寸拒貼）+ `RIG_PUMP_FULL_ONLY` 整頁幀回退開關（待拍板#2）。驗證：local-gate 全綠、純函數單測（分類/唯一匹配/無殘影/越界裁剪） |
| `2f35b6a` | 獨立審查批次 4（音視頻成本）：AV 意圖與 HLS 運行分離（M01，RTC 連通 grace 5s 後停 HLS、歸零起兜底）+ 音頻線程持有句柄並 join（M02，首幀未來即停不泄漏）+ /channels 與 /health 報 media_state/media_error（M03）。驗證：local-gate 全綠、M01 策略表單測 |
| `301382a` | 獨立審查批次 5（產品體驗與觀測）：DOM 默認保留 RTC 聲音（U01，HLS-only 明示無聲；pollAv 按 av_wanted 決策；RTC 連通切流跟到 RTC）+ DC 分片重組有界（D03，16 條/8MB/10s 過期，會話換代清空）+ region 緩存頁代次隔離與上限（D04，雙端）+ 自適應連續 2 次+3s 間隔遲滯（U03）+ /health 補 pump/rtc/dom_viewers 計數、[pump] 改 30s 定期匯總、libav 日誌收斂 Warning（O01/S6）+ README 單人併發聲明（U02/S4）。驗證：local-gate 全綠、viewer.js 語法檢查、/channels 新字段真進程 curl 驗證 |

## 14. 獨立審查整改（2026-11-20，輸入見 `plan/`）

- 待拍板三項的處置（均取報告推薦值，負責人可推翻）：① DOM 默認保留 RTC 聲音（已實現；HLS-only 無獨立聲軌仍靜音並明示）；② reflow 頻發允許回退整頁幀（`RIG_PUMP_FULL_ONLY=1`，非常駐）；③ 保持單人受信口徑（README 已聲明，見 U02）。
- 未做（需條件/另議）：V03 尺寸啟發式（高度疑似，未證實不動）；HLS 單 rendition（可選優化）；S5 HLS 音頻端到端探針、測試#4（observe_full 失敗注入）、#11（四路渲染全路徑）、#12（HLS 無首幀十次啟停）需 PulseAudio/真機環境（本機無聲卡未驗）；S7（WS 推送替代輪詢、真 shadow-DOM）列為後續；audio_rms 未接入健康（儀表缺口）。
- 架構重寫不在本輪：狀態所有者拆分（ViewerConnection/MediaCoordinator/ExecControl）只做增量——本輪的 ConnSubs/DcPending/owned guard 即按此方向落地。
| `0254b67` | 报告测试#4：ai.rs 内置可编排假 CDP（HTTP /json/version + WS 双端口，主会话事件在 setAutoAttach 后推，避 broadcast 竞态）——动作后刷新注入失败回归网（Done 必带校验帧+重观察附注）与 CDP 全挂干净拒绝；V03 帧分类加长宽比一致性（±5%）；HLS 单 rendition（`RIG_HLS_SINGLE=h264|hevc`，非法值显式报错，/channels 与 /health 暴露 `hls_renditions`，观看端按提供表选流）。验证：local-gate 全绿、单测 43/43、真进程 curl（`hls_renditions:["h264"]`） |
