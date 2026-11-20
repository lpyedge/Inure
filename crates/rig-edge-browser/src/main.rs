//! rig-edge-browser：浏览器目标的边缘程序。
//!
//! 与目标同级部署，本机/局域网直连即本体形态（中转是手段不是目的）：
//!
//! - 浏览器来源：自拉无头 Chromium，或 --cdp-url 挂靠现成 Chromium
//!   （client 与浏览器分开，边缘退出不杀挂靠的浏览器）。
//! - AI 通道前端二选一：HTTP（/observe、/act、/screenshot.png），或
//!   --stdio 的 MCP（stdin/stdout JSON-RPC，智能体子进程形态，进程
//!   边界即信任边界）。人通道（观看页/影音流）恒走 HTTP/WS。
//! - 反向接入转发中心是后续增量（待轨道 T Step 1–2），不构成使用前置。
//! - 直连鉴权：--token / RIG_EDGE_TOKEN 设令牌后 HTTP 全接口需带令牌；
//!   绑非回环地址必须设令牌，否则拒绝启动。
//! - 持久化双目录（LIU 2026-11-20 定）：--profile-dir（/ RIG_EDGE_
//!   PROFILE_DIR）指定 Chromium 用户数据目录——登录态、Cookie、
//!   LocalStorage/IndexedDB、保存的密码、扩展、应用全在其中，整目录
//!   搬到新环境即可无损续用；--cache-dir（/ RIG_EDGE_CACHE_DIR）
//!   单独放可随时重取的磁盘缓存。都不给时回落一次性临时配置。
//!
//! 双通道独立开关（省流量）：
//!
//! - 人通道（human）：screencast 视频流 + 输入转发；关 = 停投屏，零帧流量。
//! - AI 通道（ai）：CDP 无障碍树快照 + 动作；关 = 拒绝观察/动作，不做快照工作。
//!
//! 两通道可单开、双开、双关；双关时仅控制接口存活。

mod ai;
mod browser;
mod cdp;
mod chrome;
mod dom;
mod encode;
mod exec;
mod human;
mod media;
mod metrics;
mod rtc;
mod snapshot;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Request, State};
use axum::http::{header, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use futures_util::{SinkExt, StreamExt};
use rig_proto::observation::{
    AbortReason, ActionRequest, ActionResult, ObserveMode, ObserveRequest,
};
use serde::{Deserialize, Serialize};
use serde_json::json;

use ai::{AiChannel, ObserveOut};
use browser::Browser;
use exec::ExecControl;
use human::HumanChannel;

struct App {
    browser: Arc<Browser>,
    ai: AiChannel,
    human: Arc<HumanChannel>,
    media: media::MediaPipeline,
    /// WebRTC 主路 hub（Phase 1b）：共享编码 feeder + 会话工厂
    rtc: Arc<rtc::RtcHub>,
    dom: Arc<dom::DomChannel>,
    /// AI 执行状态（AI/人两通道共享同一实例）：WS 发送侧据此给
    /// DOM 模式的连接推幽灵指针位置
    exec: Arc<ExecControl>,
    /// 监听地址里的具体 IP（通配绑定时为 None）：RTC 候选地址的来源
    listen_ip: Option<std::net::IpAddr>,
    av_capable: bool,
    gate_ai: AtomicBool,
    gate_human: AtomicBool,
    /// 人通道最近一次启动失败的原因（R-11：状态必须如实——
    /// 启动失败不得把 gate 报成已启用，错误要可查询）
    human_error: std::sync::Mutex<Option<String>>,
    /// 直连鉴权令牌（--token / RIG_EDGE_TOKEN）；None = 不鉴权（仅回环允许）
    token: Option<String>,
}

/// 直连鉴权中间件：设了令牌时，除 /health 外所有接口都要带令牌——
/// `Authorization: Bearer <token>` 或 `?token=<token>`（观看页的媒体元素
/// 与 WebSocket 无法设请求头，走查询参数）。中转是手段不是目的：边缘
/// 直连本机/局域网是本体形态，鉴权是它敢绑出回环的前提（LIU 2026-11-19）。
async fn auth_guard(State(app): State<Arc<App>>, req: Request, next: Next) -> Response {
    let Some(token) = &app.token else {
        return next.run(req).await;
    };
    if req.uri().path() == "/health" || req.uri().path() == "/ready" {
        return next.run(req).await;
    }
    // /viewer.js 免鑑權：它是觀看頁的靜態腳本（無秘密、內容對
    // 所有人相同），而 <script src> 子資源請求帶不上頁面 URL
    // 裡的 ?token=——不豁免則帶令牌訪問時腳本被 401、頁面只剩
    // 空殼（第五期上線實踩：標籤/按鈕/幀全滅）。頁面本體、WS
    // 與全部 API 仍查令牌。
    if req.uri().path() == "/viewer.js" {
        return next.run(req).await;
    }
    let from_header = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "));
    let from_query = req.uri().query().and_then(|q| {
        q.split('&')
            .find_map(|kv| kv.strip_prefix("token="))
            // 查询串里的令牌是 percent-encoded 的，与原始令牌比较前必须
            // 先解码，否则含特殊字符的令牌永远认证失败（GPT review R-09）
            .map(percent_decode)
    });
    let ok = from_header == Some(token.as_str()) || from_query.as_deref() == Some(token.as_str());
    if ok {
        next.run(req).await
    } else {
        (
            StatusCode::UNAUTHORIZED,
            Json(json!({"error": "需要令牌（Authorization: Bearer 或 ?token=）"})),
        )
            .into_response()
    }
}

/// TLS 配置：--tls-cert/--tls-key 成对出现才启用；文件缺失时
/// 生成自签名证书写入给定路径。单给一个是配置错误，拒绝启动。
///
/// **非回环强制 TLS（LIU 裁定 2026-11-20，整改第二期）**：
/// WebViewer 承载 DOM 通道（含页面字段值），其安全性由传输
/// 加密保证——非回环监听未提供证书时，自动生成自签名证书
/// （~/.rig-edge/self-signed.{crt,key}）并显著提示，不再有
/// 明文形态。回环监听维持可选（浏览器视为安全上下文）。
/// 返回 None = 明文 HTTP（仅回环）。
async fn build_tls(
    get_arg: &dyn Fn(&str) -> Option<String>,
    listen: &str,
) -> Option<axum_server::tls_rustls::RustlsConfig> {
    let (cert, key) = (get_arg("--tls-cert"), get_arg("--tls-key"));
    let (cert, key) = match (cert, key) {
        (None, None) => {
            if listen_is_loopback(listen) {
                return None;
            }
            let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
            eprintln!(
                "rig-edge-browser: 非回环监听 {listen} 强制 TLS（未提供 --tls-cert/--tls-key，自动使用自签名证书）"
            );
            (
                format!("{home}/.rig-edge/self-signed.crt"),
                format!("{home}/.rig-edge/self-signed.key"),
            )
        }
        (Some(c), Some(k)) => (c, k),
        _ => {
            eprintln!("拒绝启动：--tls-cert 与 --tls-key 必须成对提供。");
            std::process::exit(2);
        }
    };
    if !std::path::Path::new(&cert).exists() || !std::path::Path::new(&key).exists() {
        let host = listen
            .rsplit_once(':')
            .map(|(h, _)| h.trim_matches(['[', ']']).to_string())
            .unwrap_or_else(|| "localhost".into());
        let mut sans = vec!["localhost".to_string(), "127.0.0.1".to_string()];
        if !sans.contains(&host) && !host.is_empty() {
            sans.push(host);
        }
        let ck = rcgen::generate_simple_self_signed(sans.clone()).expect("自签名证书生成失败");
        if let Some(dir) = std::path::Path::new(&cert).parent() {
            std::fs::create_dir_all(dir).expect("证书目录创建失败");
        }
        std::fs::write(&cert, ck.cert.pem()).expect("证书写入失败");
        // 私钥以 0600 原子创建（R-22：不留先落盘后改权限的窗口），
        // 写入与权限结果都必须成功，失败即拒绝启动
        write_key_0600(&key, ck.key_pair.serialize_pem().as_bytes());
        eprintln!(
            "rig-edge-browser: 已生成自签名证书 {cert}（SAN: {}）",
            sans.join(", ")
        );
    }
    let cfg = axum_server::tls_rustls::RustlsConfig::from_pem_file(&cert, &key)
        .await
        .expect("TLS 证书加载失败");
    Some(cfg)
}

/// 以 0600 权限创建并写入私钥文件（Unix）；任一步失败 panic
/// （启动期配置错误应当场暴露，不静默降级）
fn write_key_0600(path: &str, bytes: &[u8]) {
    #[cfg(unix)]
    {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(path)
            .expect("私钥文件创建失败");
        f.write_all(bytes).expect("私钥写入失败");
    }
    #[cfg(not(unix))]
    {
        std::fs::write(path, bytes).expect("私钥写入失败");
    }
}

/// 监听地址是否只绑回环（localhost / 回环 IP）。解析不了按非回环处理（失败安全）。
fn listen_is_loopback(listen: &str) -> bool {
    let host = listen.rsplit_once(':').map(|(h, _)| h).unwrap_or(listen);
    let host = host.trim_matches(['[', ']']);
    if host.eq_ignore_ascii_case("localhost") {
        return true;
    }
    host.parse::<std::net::IpAddr>()
        .map(|ip| ip.is_loopback())
        .unwrap_or(false)
}

#[derive(Serialize)]
struct ChannelState {
    ai: bool,
    human: bool,
    /// 音视频合流管线是否在跑
    av: bool,
    /// 本机是否具备合流条件（ffmpeg + PulseAudio 虚拟声卡）
    av_available: bool,
    /// 人通道启动失败原因（R-11）；正常时为 null
    human_error: Option<String>,
}

fn channel_state(app: &Arc<App>, av: bool) -> ChannelState {
    ChannelState {
        ai: app.gate_ai.load(Ordering::SeqCst),
        human: app.gate_human.load(Ordering::SeqCst),
        av,
        av_available: app.av_capable,
        human_error: app.human_error.lock().unwrap().clone(),
    }
}

#[derive(Deserialize, Default)]
struct ChannelSet {
    ai: Option<bool>,
    human: Option<bool>,
    av: Option<bool>,
}

fn origin_of(url: &str) -> Option<String> {
    let (scheme, rest) = url
        .strip_prefix("https://")
        .map(|r| ("https", r))
        .or_else(|| url.strip_prefix("http://").map(|r| ("http", r)))?;
    let hostport = rest.split('/').next()?;
    Some(format!("{scheme}://{hostport}"))
}

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().collect();
    let get_arg = |name: &str| -> Option<String> {
        args.windows(2).find(|w| w[0] == name).map(|w| w[1].clone())
    };
    let has = |name: &str| args.iter().any(|a| a == name);
    let listen = get_arg("--listen").unwrap_or_else(|| "127.0.0.1:39871".into());
    let start_url = get_arg("--url");
    let width: u32 = get_arg("--width")
        .and_then(|v| v.parse().ok())
        .unwrap_or(1280);
    let height: u32 = get_arg("--height")
        .and_then(|v| v.parse().ok())
        .unwrap_or(800);
    let ai_default = !has("--no-ai");
    let human_default = has("--human");
    let av_default = has("--av");
    let token = get_arg("--token").or_else(|| std::env::var("RIG_EDGE_TOKEN").ok());
    // 持久化双目录（LIU 2026-11-20 定）：profile 存一切身份与站点
    // 状态（凭据/Cookie/存储/扩展/应用），cache 只放可重取的磁盘
    // 缓存；换环境重部署时把 profile 目录整份带走即可续用。
    let profile_dir = get_arg("--profile-dir")
        .or_else(|| std::env::var("RIG_EDGE_PROFILE_DIR").ok())
        .filter(|s| !s.is_empty());
    let cache_dir = get_arg("--cache-dir")
        .or_else(|| std::env::var("RIG_EDGE_CACHE_DIR").ok())
        .filter(|s| !s.is_empty());
    // TLS（LIU 2026-11-20 定）：--tls-cert/--tls-key 成对提供即以
    // HTTPS 服务；文件不存在时按监听地址自动生成自签名证书（SAN
    // 含监听 IP、127.0.0.1 与 localhost）。局域网观看页只有走
    // HTTPS 才是安全上下文，剪贴板 writeText/ClipboardItem 等
    // 标准路径才可用——不再为非安全上下文绕路。
    let tls = build_tls(&get_arg, &listen).await;
    // 直连安全底线：绑出回环（局域网可达）必须设令牌，否则拒绝启动——
    // 无鉴权的边缘等于把浏览器操作权开放给同网段任何人。
    if token.is_none() && !listen_is_loopback(&listen) {
        eprintln!(
            "拒绝启动：监听 {listen} 不是回环地址且未设令牌。局域网直连请加 --token <令牌>（或环境变量 RIG_EDGE_TOKEN）。"
        );
        std::process::exit(2);
    }

    // 虚拟声卡先行：Chromium 出生时就把声音放进 null sink，
    // 之后 ffmpeg 从它的 monitor 合流（LIU 2026-11-18 定的音频方案）。
    let av_capable = media::ensure_virtual_sink().await;
    if av_capable {
        std::env::set_var("PULSE_SINK", media::SINK_NAME);
    }

    // 浏览器来源二选一：--cdp-url 挂靠现成 Chromium（client 与浏览器分开，
    // 边缘退出不杀它），或自拉一个无头 Chromium。
    let attached = get_arg("--cdp-url");
    let browser = if let Some(u) = &attached {
        Browser::attach(u).await.expect("挂靠 Chromium 失败")
    } else {
        Browser::launch(
            get_arg("--chromium").as_deref(),
            width,
            height,
            start_url.as_deref(),
            profile_dir.as_deref(),
            cache_dir.as_deref(),
        )
        .await
        .expect("启动 Chromium 失败")
    };
    if attached.is_none() {
        eprintln!(
            "rig-edge-browser: profile={} cache={}",
            profile_dir.as_deref().unwrap_or("(临时)"),
            cache_dir.as_deref().unwrap_or("(profile 内)"),
        );
    }
    if let Some(url) = &start_url {
        if url != "about:blank" {
            if let Some(o) = origin_of(url) {
                browser.grant_clipboard(&o).await;
            }
            // 自拉模式下起始 URL 已是初始标签页（见 launch），
            // 不再 navigate（避免历史里多一条 about:blank）；
            // 挂靠模式才需要把现成浏览器带到起始页。
            if attached.is_some() {
                let main = browser.main().await.expect("主会话");
                let _ = browser.navigate(&main, url).await;
                tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            }
        }
    }
    let mut origins: Vec<String> = Vec::new();
    if let Some(u) = &start_url {
        if let Some(o) = origin_of(u) {
            origins.push(o);
        }
    }
    let mut i = 0;
    while i < args.len() {
        if args[i] == "--allow-origin" {
            if let Some(v) = args.get(i + 1) {
                origins.push(v.clone());
            }
        }
        i += 1;
    }
    // --stun <host:port>（可重复）：配了则 RTC 会话走全 ICE + srflx
    // 采集（跨子网 P2P 直连档）；解析失败的条目警告后跳过
    let mut stun: Vec<std::net::SocketAddr> = Vec::new();
    let mut i = 0;
    while i < args.len() {
        if args[i] == "--stun" {
            if let Some(v) = args.get(i + 1) {
                use std::net::ToSocketAddrs;
                match v.to_socket_addrs().and_then(|mut it| {
                    it.next().ok_or_else(|| {
                        std::io::Error::new(std::io::ErrorKind::NotFound, "无解析结果")
                    })
                }) {
                    Ok(a) => stun.push(a),
                    Err(e) => eprintln!("[rtc] --stun {v} 解析失败，跳过: {e}"),
                }
            }
        }
        i += 1;
    }

    let exec = Arc::new(ExecControl::default());
    let human = Arc::new(HumanChannel::new(browser.clone(), exec.clone()));
    let listen_ip = listen
        .rsplit_once(':')
        .map(|(h, _)| h.trim_matches(['[', ']']))
        .and_then(|h| h.parse::<std::net::IpAddr>().ok())
        .filter(|ip| !ip.is_unspecified());
    let app = Arc::new(App {
        ai: AiChannel::new(browser.clone(), origins, exec.clone()),
        exec,
        media: media::MediaPipeline::new(human.clone()),
        rtc: rtc::RtcHub::new(human.clone(), stun),
        dom: dom::DomChannel::new(browser.clone(), human.clone()),
        listen_ip,
        human,
        browser,
        av_capable,
        gate_ai: AtomicBool::new(ai_default),
        gate_human: AtomicBool::new(human_default),
        human_error: std::sync::Mutex::new(None),
        token,
    });
    app.human.set_gate_display(ai_default, human_default).await;
    if human_default {
        if let Err(e) = app.human.start().await {
            // R-11：启动失败如实降级——gate 不报已启用，错误可查
            eprintln!("人通道启动失败: {e}");
            app.gate_human.store(false, Ordering::SeqCst);
            *app.human_error.lock().unwrap() = Some(e);
            app.human.set_gate_display(ai_default, false).await;
        }
        if av_default && av_capable {
            if let Err(e) = app.media.start().await {
                eprintln!("音视频合流启动失败: {e}");
            }
        }
    }

    let router = Router::new()
        .route("/", get(viewer))
        .route("/viewer.js", get(viewer_js))
        .route("/health", get(health))
        .route("/ready", get(ready))
        .route("/channels", get(get_channels).post(set_channels))
        .route("/observe", post(observe))
        .route("/act", post(act))
        .route("/screenshot.png", get(screenshot))
        .route("/print.pdf", get(print_pdf))
        .route("/stream/{rend}/{name}", get(stream_file))
        .route("/ws", get(ws_handler))
        .layer(middleware::from_fn_with_state(app.clone(), auth_guard))
        .with_state(app.clone());

    if has("--stdio") {
        // stdio 模式：AI 通道走 MCP（stdin/stdout），进程边界即信任边界；
        // 人通道要开时 HTTP 并行跑（仅供观看页）。stdout 只写协议帧，
        // 本分支的提示一律走 stderr。stdin 关闭（智能体退出）即收尾。
        let http = if human_default {
            let addr: std::net::SocketAddr = listen.parse().expect("监听地址无效");
            let svc = router.into_make_service_with_connect_info::<std::net::SocketAddr>();
            eprintln!(
                "rig-edge-browser: stdio (MCP) + human viewer on {}://{listen}",
                if tls.is_some() { "https" } else { "http" }
            );
            Some(tokio::spawn(async move {
                let _ = match tls {
                    Some(cfg) => axum_server::bind_rustls(addr, cfg).serve(svc).await,
                    None => axum_server::bind(addr).serve(svc).await,
                };
            }))
        } else {
            eprintln!("rig-edge-browser: stdio (MCP) mode");
            None
        };
        {
            // 信号收尾与 HTTP 分支同口径：SIGTERM/SIGINT 时先请
            // Chromium 落盘关闭再退出，不等 stdin 关闭
            let app_sig = app.clone();
            tokio::spawn(async move {
                let mut term =
                    tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                        .expect("注册 SIGTERM 处理失败");
                tokio::select! {
                    _ = tokio::signal::ctrl_c() => {}
                    _ = term.recv() => {}
                }
                app_sig.browser.shutdown().await;
                std::process::exit(0);
            });
        }
        serve_stdio(app.clone()).await;
        if let Some(h) = http {
            h.abort();
        }
    } else {
        let addr: std::net::SocketAddr = listen.parse().expect("监听地址无效");
        let svc = router.into_make_service_with_connect_info::<std::net::SocketAddr>();
        let handle = axum_server::Handle::new();
        {
            let h = handle.clone();
            tokio::spawn(async move {
                // SIGINT 与 SIGTERM 都走优雅退出：serve 返回后
                // main 末尾的 browser.shutdown() 会请 Chromium
                // 落盘关闭（持久化目录靠它兑现，见 browser.rs）。
                // 只认 ctrl_c 时，systemd/部署脚本的 kill（TERM）
                // 会把边缘连同未落盘的登录态一起带走。
                let mut term =
                    tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                        .expect("注册 SIGTERM 处理失败");
                tokio::select! {
                    _ = tokio::signal::ctrl_c() => {}
                    _ = term.recv() => {}
                }
                h.graceful_shutdown(Some(std::time::Duration::from_secs(5)));
            });
        }
        println!(
            "rig-edge-browser listening on {}://{listen} (ai={ai_default}, human={human_default})",
            if tls.is_some() { "https" } else { "http" }
        );
        let r = match tls {
            Some(cfg) => {
                axum_server::bind_rustls(addr, cfg)
                    .handle(handle)
                    .serve(svc)
                    .await
            }
            None => axum_server::bind(addr).handle(handle).serve(svc).await,
        };
        r.expect("服务异常退出");
    }
    app.browser.shutdown().await;
}

async fn viewer() -> impl axum::response::IntoResponse {
    /* no-store：观看页随边缘版本演进，浏览器缓存住旧版页面/
    脚本会让已修的状态机在用户侧继续跑旧逻辑（且无任何
    提示）——每次加载都取当前构建的版本 */
    (
        [(axum::http::header::CACHE_CONTROL, "no-store")],
        Html(VIEWER_HTML),
    )
}

async fn viewer_js() -> impl axum::response::IntoResponse {
    (
        [
            (
                axum::http::header::CONTENT_TYPE,
                "text/javascript; charset=utf-8",
            ),
            (axum::http::header::CACHE_CONTROL, "no-store"),
        ],
        VIEWER_JS,
    )
}

async fn health(State(app): State<Arc<App>>) -> Json<serde_json::Value> {
    let sessions = app.browser.sessions.lock().await.len();
    let (fw, fh) = app.human.last_size().await;
    // R-19：liveness（ok 恒 true）之外附 readiness 与可观测字段
    // （F-09 计量面）：CDP 存活、帧龄、在途数、各段延迟分位、
    // 丢帧计数、最近一次快照量测（R-16）
    let m = metrics::global();
    let cdp_connected = app.browser.cdp.is_alive();
    let last_snapshot = m
        .last_observe
        .lock()
        .unwrap()
        .as_ref()
        .map(|o| {
            json!({
                "total_ms": o.total_ms,
                "cdp_calls": o.cdp_calls,
                "cdp_ms": o.cdp_ms,
                "quads_calls": o.quads_calls,
                "quads_ms": o.quads_ms,
                "ax_dom_ms": o.ax_dom_ms,
                "nodes": o.nodes,
            })
        })
        .unwrap_or(serde_json::Value::Null);
    Json(json!({
        "ok": true,
        "ready": cdp_connected && sessions > 0,
        "ai": app.gate_ai.load(Ordering::SeqCst),
        "human": app.gate_human.load(Ordering::SeqCst),
        "human_running": app.human.is_running(),
        "frame_size": [fw, fh],
        "sessions": sessions,
        "cdp_connected": cdp_connected,
        "last_frame_age_ms": app.human.last_frame_age_ms(),
        "exec_in_flight": app.ai.exec_in_flight().await,
        "frames": {
            "sent": m.frames_sent.load(Ordering::Relaxed),
            "skipped": m.frames_skipped.load(Ordering::Relaxed),
        },
        "latency": {
            "compose": m.compose.snapshot().to_json(),
            "encode": m.encode.snapshot().to_json(),
            "act": m.act.snapshot().to_json(),
            "observe": m.observe.snapshot().to_json(),
            "dom_snapshot": m.dom_flush.snapshot().to_json(),
            "quads": m.quads.snapshot().to_json(),
        },
        "last_snapshot": last_snapshot,
    }))
}

/// readiness 探针：CDP 断开或无会话时 503（与 /health 的
/// liveness 分离，R-19）。不带任何敏感信息，免鉴权
async fn ready(State(app): State<Arc<App>>) -> (axum::http::StatusCode, Json<serde_json::Value>) {
    let sessions = app.browser.sessions.lock().await.len();
    let cdp_connected = app.browser.cdp.is_alive();
    let ok = cdp_connected && sessions > 0;
    (
        if ok {
            axum::http::StatusCode::OK
        } else {
            axum::http::StatusCode::SERVICE_UNAVAILABLE
        },
        Json(json!({
            "ready": ok,
            "cdp_connected": cdp_connected,
            "sessions": sessions,
        })),
    )
}

async fn get_channels(State(app): State<Arc<App>>) -> Json<ChannelState> {
    let av = app.media.running().await;
    Json(channel_state(&app, av))
}

/// 应用通道开关（HTTP /channels 与观看端菜单共用同一逻辑），
/// 并同步观看端菜单里的勾选态显示。
async fn apply_gate_flags(app: &Arc<App>, ai: Option<bool>, human: Option<bool>) {
    if let Some(h) = human {
        let cur = app.gate_human.load(Ordering::SeqCst);
        if h && !cur {
            // R-11：start 成功才置 gate；失败保留关闭并记录原因
            match app.human.start().await {
                Ok(()) => {
                    *app.human_error.lock().unwrap() = None;
                    app.gate_human.store(true, Ordering::SeqCst);
                }
                Err(e) => {
                    eprintln!("人通道启动失败: {e}");
                    *app.human_error.lock().unwrap() = Some(e);
                }
            }
        } else if !h && cur {
            app.media.stop().await;
            let _ = app.human.stop().await;
            app.gate_human.store(false, Ordering::SeqCst);
        }
    }
    if let Some(a) = ai {
        app.gate_ai.store(a, Ordering::SeqCst);
    }
    app.human
        .set_gate_display(
            app.gate_ai.load(Ordering::SeqCst),
            app.gate_human.load(Ordering::SeqCst),
        )
        .await;
}

async fn set_channels(
    State(app): State<Arc<App>>,
    Json(set): Json<ChannelSet>,
) -> Json<ChannelState> {
    apply_gate_flags(&app, set.ai, set.human).await;
    if let Some(av) = set.av {
        if av && app.av_capable && app.gate_human.load(Ordering::SeqCst) {
            let _ = app.media.start().await;
        } else if !av {
            app.media.stop().await;
        }
    }
    let av = app.media.running().await;
    Json(channel_state(&app, av))
}

/// 观看端「打印」菜单的产物取回（打印一次取一次，取后清空）
async fn print_pdf(State(app): State<Arc<App>>) -> Response {
    if !app.gate_human.load(Ordering::SeqCst) {
        return channel_off();
    }
    match app.human.take_print_pdf().await {
        Some(bytes) => ([(header::CONTENT_TYPE, "application/pdf")], bytes).into_response(),
        None => (StatusCode::NOT_FOUND, "no print output").into_response(),
    }
}

/// HLS 合流输出：/stream/{hevc|h264}/index.m3u8、init.mp4、seg-N.m4s
async fn stream_file(
    State(app): State<Arc<App>>,
    axum::extract::Path((rend, name)): axum::extract::Path<(String, String)>,
) -> Response {
    if !app.gate_human.load(Ordering::SeqCst) {
        return channel_off();
    }
    match app.media.read_file(&rend, &name).await {
        Some(bytes) => {
            let ct = if name.ends_with(".m3u8") {
                "application/vnd.apple.mpegurl"
            } else if name.ends_with(".mp4") {
                "video/mp4"
            } else {
                "video/iso.segment"
            };
            // 设了令牌时给播放列表里的相对段地址补 ?token=：媒体元素解析
            // 相对 URL 时不继承播放列表 URL 的查询串，段请求会丢令牌。
            let bytes = if name.ends_with(".m3u8") {
                match &app.token {
                    Some(t) => rewrite_playlist_tokens(&bytes, t).into_bytes(),
                    None => bytes,
                }
            } else {
                bytes
            };
            (
                StatusCode::OK,
                [(axum::http::header::CONTENT_TYPE, ct)],
                bytes,
            )
                .into_response()
        }
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

/// 给 HLS 播放列表中的段/初始化段 URI 追加 ?token=（见 stream_file 注释）。
fn rewrite_playlist_tokens(bytes: &[u8], token: &str) -> String {
    let enc: String = token
        .bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect();
    let suffix = format!("?token={enc}");
    String::from_utf8_lossy(bytes)
        .lines()
        .map(|line| {
            if line.starts_with('#') {
                // #EXT-X-MAP:URI="init.mp4" 形式
                if line.contains("URI=\"") && !line.contains("?token=") {
                    line.replacen(".mp4\"", &format!(".mp4{suffix}\""), 1)
                } else {
                    line.to_string()
                }
            } else if line.is_empty() || line.contains("?token=") {
                line.to_string()
            } else {
                format!("{line}{suffix}")
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
        + "\n"
}

fn channel_off() -> Response {
    (
        StatusCode::CONFLICT,
        Json(json!({"error": "channel disabled"})),
    )
        .into_response()
}

async fn observe(State(app): State<Arc<App>>, Json(req): Json<ObserveRequest>) -> Response {
    if !app.gate_ai.load(Ordering::SeqCst) {
        return channel_off();
    }
    // mode 决定响应组成：text 只要文本视图；tree 树+文本；
    // tree_image 再加编号截图（base64 PNG）。budget/subtree/
    // include_offscreen 在 AiChannel 里作用于响应副本（见
    // observe_req）；since_frame 命中时回增量（delta 字段）。
    match app.ai.observe_req(&req).await {
        Ok(ObserveOut::Delta(delta, text)) => {
            Json(json!({"delta": delta, "text": text})).into_response()
        }
        Ok(ObserveOut::Full(obs, text, image)) => match req.mode {
            ObserveMode::Text => Json(json!({"text": text})).into_response(),
            ObserveMode::Tree => Json(json!({"observation": obs, "text": text})).into_response(),
            ObserveMode::TreeImage => {
                use base64::Engine;
                let b64 = image
                    .as_deref()
                    .map(|p| base64::engine::general_purpose::STANDARD.encode(p))
                    .unwrap_or_default();
                Json(json!({"observation": obs, "text": text, "image_png_base64": b64}))
                    .into_response()
            }
        },
        Err(e) => {
            let code = if e.starts_with("subtree ") {
                StatusCode::BAD_REQUEST
            } else {
                StatusCode::INTERNAL_SERVER_ERROR
            };
            (code, Json(json!({"error": e}))).into_response()
        }
    }
}

async fn act(State(app): State<Arc<App>>, Json(req): Json<ActionRequest>) -> Response {
    if !app.gate_ai.load(Ordering::SeqCst) {
        return channel_off();
    }
    let result = app.ai.act(req).await;
    Json(json!({"result": result})).into_response()
}

async fn screenshot(State(app): State<Arc<App>>) -> Response {
    if !app.gate_ai.load(Ordering::SeqCst) {
        return channel_off();
    }
    match app.ai.marked_screenshot().await {
        Ok(png) => ([(header::CONTENT_TYPE, "image/png")], png).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": e}))).into_response(),
    }
}

async fn ws_handler(
    State(app): State<Arc<App>>,
    ws: WebSocketUpgrade,
    axum::extract::ConnectInfo(remote): axum::extract::ConnectInfo<std::net::SocketAddr>,
) -> Response {
    if !app.gate_human.load(Ordering::SeqCst) {
        return channel_off();
    }
    ws.on_upgrade(move |socket| ws_session(app, socket, remote))
}

/// RTC host 候选的通告地址：监听地址是具体 IP 时用它；绑通配地址时
/// 以观看端的远端地址探出站接口 IP（回环/局域网形态足够；全 ICE 在 P2P 档）。
fn candidate_ip(app: &App, remote: std::net::SocketAddr) -> std::net::IpAddr {
    if let Some(ip) = app.listen_ip {
        return ip;
    }
    if remote.ip().is_loopback() {
        return remote.ip();
    }
    let probe = std::net::UdpSocket::bind(if remote.is_ipv6() {
        "[::]:0"
    } else {
        "0.0.0.0:0"
    })
    .and_then(|s| {
        s.connect(remote)?;
        s.local_addr()
    });
    match probe {
        Ok(a) => a.ip(),
        Err(_) => remote.ip(),
    }
}

/// 帧的二进制线格式：8 字节头（宽、高，u32 小端）+ JPEG 原始字节。
/// 控制消息仍走 JSON 文本；帧走二进制，省掉 base64 的 +33% 体积与两端编解码。
fn frame_message(f: &human::FrameMsg) -> Message {
    let mut buf = Vec::with_capacity(8 + f.jpeg.len());
    buf.extend_from_slice(&f.width.to_le_bytes());
    buf.extend_from_slice(&f.height.to_le_bytes());
    buf.extend_from_slice(&f.jpeg);
    Message::Binary(buf.into())
}

/// D01/D02：单连接订阅记账——`acquire_*`/`release_*` 各自幂等，
/// 重复 start/stop 不改变远端计数；真正的释放只发生在父作用域
/// （`ws_session` 尾部），任一侧任务先结束（含 abort）都不丢清理。
#[derive(Debug, Default)]
struct ConnSubs {
    dom_held: bool,
    jpeg_held: bool,
}

impl ConnSubs {
    fn new() -> Self {
        // JPEG 需求连接建立即持有（视频模式默认）；DOM 按需进入
        ConnSubs {
            dom_held: false,
            jpeg_held: true,
        }
    }
    /// 进入 DOM：未持有才返回 true（调用方据此调一次 viewer_start）
    fn acquire_dom(&mut self) -> bool {
        if self.dom_held {
            false
        } else {
            self.dom_held = true;
            true
        }
    }
    /// 退出 DOM：持有才返回 true（调用方据此调一次 viewer_stop）
    fn release_dom(&mut self) -> bool {
        if self.dom_held {
            self.dom_held = false;
            true
        } else {
            false
        }
    }
    /// 释放 JPEG 需求：持有才返回 true（调用方据此调一次 demand_add(-1)）
    fn release_jpeg(&mut self) -> bool {
        if self.jpeg_held {
            self.jpeg_held = false;
            true
        } else {
            false
        }
    }
}

async fn ws_session(app: Arc<App>, socket: WebSocket, remote: std::net::SocketAddr) {
    let (mut sink, mut stream) = socket.split();
    // hello：告知 CSS 视口尺寸，观看端据此换算输入坐标
    if let Ok(main) = app.browser.main().await {
        if let Ok((w, h, _)) = app.browser.layout(&main).await {
            let _ = sink
                .send(Message::Text(
                    json!({"type": "hello", "cssWidth": w, "cssHeight": h})
                        .to_string()
                        .into(),
                ))
                .await;
        }
    }
    // 先回放最近一帧，新观看者立刻看到当前画面
    if let Some(f) = app.human.last_frame().await {
        let _ = sink.send(frame_message(&f)).await;
    }
    let mut frames = app.human.subscribe();
    let mut notices = app.human.subscribe_notices();
    let mut dom_msgs = app.dom.subscribe();
    let app2 = app.clone();
    // D01/D02：本连接的订阅记账（跨两个任务共享，释放只在父作用域）。
    // JPEG 需求连接建立即持有，见 ConnSubs::new。
    let subs = Arc::new(std::sync::Mutex::new(ConnSubs::new()));
    let subs2 = subs.clone();
    // 本连接的 transport 模式：RTC 连通后置 true，发送侧停发 WS 帧
    // （广播照常接收只丢弃）；RTC 断开/关闭自动恢复 WS 帧
    let rtc_active = Arc::new(AtomicBool::new(false));
    let rtc_on = rtc_active.clone();
    // DOM 镜像模式同理停发二进制帧（镜像走文本消息）
    let dom_active = Arc::new(AtomicBool::new(false));
    let dom_on = dom_active.clone();
    // 本连接的 rig-dom 数据通道已打开时，DOM 消息改走 DC：
    // 发送侧跳过 WS 文本副本（会话任务用自己的订阅经 DC 送达）
    let dom_dc_open = Arc::new(AtomicBool::new(false));
    let dom_dc_on = dom_dc_open.clone();
    // 额外的出站消息（如剪贴板读回）与帧流共用一个发送端
    let (out_tx, mut out_rx) = tokio::sync::mpsc::channel::<String>(16);
    let exec_ghost = app.exec.clone();
    let human2 = app.human.clone();
    let mut send_task = tokio::spawn(async move {
        // 连接建立先直发一份当前 chrome 状态（原生工具栏的初值；
        // 后续变化经 notices 广播的 chrome-state 到达）
        let init_state = human2.state_json_string().await;
        if sink.send(Message::Text(init_state.into())).await.is_err() {
            return;
        }
        // AI 幽灵指针（第五期起全模式）：指针不再合成进帧——
        // 150ms 轮询执行状态，位置/标签/轨迹变化时经 WS 文本
        // 下发，观看端在当前显示面上层用 overlay 画。
        let mut ghost_tick = tokio::time::interval(std::time::Duration::from_millis(150));
        let mut ghost_last: Option<String> = None;
        loop {
            tokio::select! {
                _ = ghost_tick.tick() => {
                    let msg = exec_ghost.pointer_vis().await.map(|v| {
                        serde_json::json!({
                            "type": "ghost", "x": v.x, "y": v.y,
                            "label": v.label, "pulse": v.pulse.is_some(),
                            "trail": v.trail.iter().map(|(x, y, a)| {
                                serde_json::json!([x, y, a])
                            }).collect::<Vec<_>>(),
                        })
                        .to_string()
                    });
                    if msg != ghost_last {
                        let out = msg
                            .clone()
                            .unwrap_or_else(|| "{\"type\":\"ghost-off\"}".to_string());
                        if sink.send(Message::Text(out.into())).await.is_err() {
                            break;
                        }
                        ghost_last = msg;
                    }
                },
                f = frames.recv() => match f {
                    Ok(f) => {
                        if dom_on.load(Ordering::SeqCst) {
                            // DOM 模式不送二进制帧（镜像走文本消息；
                            // 工具栏是观看页原生 DOM，不再有条带帧）
                        } else if rtc_on.load(Ordering::SeqCst) {
                            // RTC 接管画面：WS 帧停发（广播照常收，只丢弃）
                        } else if sink.send(frame_message(&f)).await.is_err() {
                            break;
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                },
                extra = out_rx.recv() => match extra {
                    Some(text) => {
                        if sink.send(Message::Text(text.into())).await.is_err() {
                            break;
                        }
                    }
                    None => break,
                },
                note = notices.recv() => match note {
                    Ok(text) => {
                        if sink.send(Message::Text(text.into())).await.is_err() {
                            break;
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                },
                dm = dom_msgs.recv() => match dm {
                    Ok(text) => {
                        if dom_dc_on.load(Ordering::SeqCst) {
                            continue; // 已由本连接的数据通道送达
                        }
                        if sink.send(Message::Text(text.into())).await.is_err() {
                            break;
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                },
            }
        }
    });
    let mut recv_task = tokio::spawn(async move {
        let mut rtc_session: Option<rtc::SessionHandle> = None;
        // F-03：本连接的 JPEG 需求记账——默认持有（视频模式），
        // 观看端切到 RTC/DOM 时发 jpeg-demand{on:false} 释放，
        // 连接结束时父作用域兜底释放（D02）
        app2.human.jpeg_demand_add(1);
        // R-10：收包按帧型分发——Ping/Pong/Binary 不结束会话
        // （此前 while let 只认 Text，任意保活帧都会拆连接）
        while let Some(msg) = stream.next().await {
            let text = match msg {
                Ok(Message::Text(t)) => t,
                Ok(Message::Ping(_)) | Ok(Message::Pong(_)) | Ok(Message::Binary(_)) => continue,
                Ok(Message::Close(_)) | Err(_) => break,
            };
            let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) else {
                continue;
            };
            // RTC 信令在本层截获，其余消息才是人通道输入
            match v.get("type").and_then(|t| t.as_str()) {
                Some("jpeg-demand") => {
                    let on = v.get("on").and_then(|b| b.as_bool()).unwrap_or(true);
                    // D01：同一连接重复开关只记一次账
                    let op = {
                        let mut s = subs2.lock().unwrap();
                        if on && !s.jpeg_held {
                            s.jpeg_held = true;
                            1
                        } else if !on && s.release_jpeg() {
                            -1
                        } else {
                            0
                        }
                    };
                    if op != 0 {
                        app2.human.jpeg_demand_add(op);
                    }
                }
                Some("rtc-start") => {
                    rtc_session.take(); // drop 旧会话（其驱动任务随之退出）
                    let ip = candidate_ip(&app2, remote);
                    rtc_session = Some(
                        app2.rtc
                            .start_session(
                                out_tx.clone(),
                                rtc_active.clone(),
                                ip,
                                Some(app2.dom.subscribe()),
                                dom_dc_open.clone(),
                            )
                            .await,
                    );
                }
                Some("rtc-answer") => {
                    if let (Some(s), Some(sdp)) =
                        (&rtc_session, v.get("sdp").and_then(|x| x.as_str()))
                    {
                        s.answer(sdp.to_string()).await;
                    }
                }
                Some("rtc-candidate") => {
                    // candidate 字段是 RTCIceCandidate 的 JSON 对象，原样透传给会话
                    if let (Some(s), Some(c)) = (&rtc_session, v.get("candidate")) {
                        s.candidate(c.to_string()).await;
                    }
                }
                Some("rtc-stop") => {
                    rtc_session.take();
                }
                Some("dom-start") => {
                    dom_active.store(true, Ordering::SeqCst);
                    // D01：同一连接重复 dom-start 只订阅一次（自动感知+
                    // 前台显示共用一份订阅，不重复发 START）
                    let start = subs2.lock().unwrap().acquire_dom();
                    if start {
                        app2.dom.viewer_start().await;
                    }
                }
                Some("dom-stop") => {
                    dom_active.store(false, Ordering::SeqCst);
                    let stop = subs2.lock().unwrap().release_dom();
                    if stop {
                        app2.dom.viewer_stop().await;
                    }
                }
                Some("dom-resync") => {
                    app2.dom.resync().await;
                }
                _ => {
                    // DOM 模式的输入带 dom:1，坐标是页面坐标，走直达分发
                    if v.get("dom").and_then(|x| x.as_bool()) == Some(true) {
                        app2.human.handle_ws_dom(&v).await;
                        continue;
                    }
                    // 输入分发全在人通道（chrome 命中/编辑器/遮蔽接管）；
                    // 菜单里的通道开关以返回值带回这里应用 gate
                    if let Some((a, h)) = app2.human.handle_ws(&v, &out_tx).await {
                        apply_gate_flags(&app2, Some(a), Some(h)).await;
                    }
                }
            }
        }
        // D02：本任务尾部不再做释放——发送侧先结束时本任务会被 abort，
        // 尾部代码跑不到。释放统一由父作用域做（见 select 之后）。
    });
    tokio::select! {
        _ = &mut send_task => {
            recv_task.abort();
            let _ = recv_task.await;
        }
        _ = &mut recv_task => {
            send_task.abort();
            let _ = send_task.await;
        }
    }
    // D02 唯一释放点：任一侧先结束（含 abort），订阅都在这里释放一次。
    // 连接断开时（关标签页/崩溃不会发 dom-stop）名额同样回收，否则
    // 后来者的 dom-start 会被当成「后来者」而不再启动页内观察者。
    // 守卫只在同步段持有（MutexGuard 不跨 await，保证 future Send）。
    let (stop_dom, drop_jpeg) = {
        let mut s = subs.lock().unwrap();
        (s.release_dom(), s.release_jpeg())
    };
    if stop_dom {
        app.dom.viewer_stop().await;
    }
    if drop_jpeg {
        app.human.jpeg_demand_add(-1);
    }
}

const VIEWER_HTML: &str = include_str!("viewer.html");
const VIEWER_JS: &str = include_str!("viewer.js");

// ───────────────────────────── MCP stdio 前端 ─────────────────────────────
//
// 与 AI 智能体同机部署时，智能体把边缘当子进程拉起，AI 通道走 stdin/stdout
// 的 MCP（JSON-RPC 行分隔）——进程边界即信任边界，无端口无令牌（LIU
// 2026-11-19 定：stdio 对齐 MCP）。stdout 只写协议帧，日志一律走 stderr。
// stdin 关闭即退出主循环，main 随后收尾（自拉的浏览器关掉，挂靠的不动）。

async fn serve_stdio(app: Arc<App>) {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
    let mut lines = tokio::io::BufReader::new(tokio::io::stdin()).lines();
    let mut out = tokio::io::BufWriter::new(tokio::io::stdout());
    while let Ok(Some(line)) = lines.next_line().await {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(req) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        // 无 id 的是通知（如 notifications/initialized），按协议不回
        let Some(id) = req.get("id").cloned() else {
            continue;
        };
        let method = req
            .get("method")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("")
            .to_string();
        let params = req
            .get("params")
            .cloned()
            .unwrap_or(serde_json::Value::Null);
        let (msg, taken_over) = match mcp_dispatch(&app, &method, &params).await {
            Ok((result, taken)) => (json!({"jsonrpc": "2.0", "id": id, "result": result}), taken),
            Err((code, message)) => (
                json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}}),
                false,
            ),
        };
        let _ = out.write_all(msg.to_string().as_bytes()).await;
        let _ = out.write_all(b"\n").await;
        if taken_over {
            // §7.3：用户强制接管后，除在途 call 的 Aborted 结果外，
            // 再发明文通知，智能体据此停下当前指令等新指令
            let note = json!({
                "jsonrpc": "2.0",
                "method": "notifications/rig/control_taken_over",
                "params": {"reason": "user_takeover"}
            });
            let _ = out.write_all(note.to_string().as_bytes()).await;
            let _ = out.write_all(b"\n").await;
        }
        let _ = out.flush().await;
    }
}

/// 返回 (结果, 是否发生了用户强制接管)
async fn mcp_dispatch(
    app: &Arc<App>,
    method: &str,
    params: &serde_json::Value,
) -> Result<(serde_json::Value, bool), (i64, String)> {
    match method {
        "initialize" => {
            let v = params
                .get("protocolVersion")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("2025-03-26");
            Ok((
                json!({
                    "protocolVersion": v,
                    "capabilities": {"tools": {"listChanged": false}},
                    "serverInfo": {"name": "rig-edge-browser", "version": env!("CARGO_PKG_VERSION")},
                }),
                false,
            ))
        }
        "ping" => Ok((json!({}), false)),
        "tools/list" => Ok((json!({"tools": mcp_tools()}), false)),
        "tools/call" => Ok(mcp_call(app, params).await),
        _ => Err((-32601, format!("未知方法: {method}"))),
    }
}

fn mcp_tools() -> serde_json::Value {
    json!([
        {
            "name": "observe",
            "description": "观察当前页面：返回紧凑文本渲染与完整语义快照（Observation JSON）。动手前先观察，动作引用快照里的元素 id 与 frame_id。可选参数（ObserveRequest）：mode='text' 只回文本视图（最省）、'tree' 树+文本（默认）、'tree_image' 再附编号截图；budget_tokens 按预算裁剪响应视图（被裁元素仍可凭 id 操作）；subtree 只回某元素的子树；include_offscreen=true 才带视口外元素（默认不带）；since_frame 传上次帧号且仍可达时回增量（摘要+ObservationDelta JSON 两个文本块，用 apply_delta 更新本地树，base 不匹配时回完整快照）。",
            "inputSchema": {"type": "object", "properties": {"mode": {"type": "string", "enum": ["text", "tree", "tree_image"]}, "budget_tokens": {"type": "integer", "minimum": 0}, "subtree": {"type": "string"}, "since_frame": {"type": "integer", "minimum": 0}, "include_offscreen": {"type": "boolean"}}, "additionalProperties": false}
        },
        {
            "name": "act",
            "description": "执行一个动作。参数为 ActionRequest：{frame_id, action}。action 示例：{kind:'click', target:{by:'element', id:'e3'}}、{kind:'type', target:{by:'element', id:'e3'}, text:'内容', submit:false}、{kind:'key', key:'Control+A'}、{kind:'navigate', url:'https://…'}、{kind:'scroll', dx:0, dy:600}、{kind:'upload', target:{by:'element', id:'e3'}, files:['/abs/path/a.pdf']}（给文件框喂边缘主机上的文件）。frame_id 用最近一次 observe 的帧号，页面变化后陈旧帧会被拒（重新 observe 即可）。",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "frame_id": {"type": "integer"},
                    "action": {"type": "object"}
                },
                "required": ["frame_id", "action"],
                "additionalProperties": false
            }
        },
        {
            "name": "screenshot",
            "description": "截取当前页面的编号 PNG 截图（敏感区域已涂黑）。无语义区域（canvas 等）兜底用。",
            "inputSchema": {"type": "object", "properties": {}, "additionalProperties": false}
        }
    ])
}

/// 返回 (结果, 是否被用户强制接管)
async fn mcp_call(app: &Arc<App>, params: &serde_json::Value) -> (serde_json::Value, bool) {
    let name = params
        .get("name")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("");
    let args = params
        .get("arguments")
        .cloned()
        .unwrap_or(serde_json::Value::Null);
    let err = |m: String| json!({"content": [{"type": "text", "text": m}], "isError": true});
    if !app.gate_ai.load(Ordering::SeqCst) {
        return (err("AI 通道已关闭".into()), false);
    }
    let (result, taken) = match name {
        "observe" => {
            let oreq: ObserveRequest = if args.is_null() {
                ObserveRequest::default()
            } else {
                match serde_json::from_value(args.clone()) {
                    Ok(r) => r,
                    Err(e) => return (err(format!("observe 参数不合协议: {e}")), false),
                }
            };
            match app.ai.observe_req(&oreq).await {
                Ok(ObserveOut::Delta(delta, text)) => {
                    // 增量：摘要 + 增量本体 JSON 两个文本块
                    let content = vec![
                        json!({"type": "text", "text": text}),
                        json!({"type": "text", "text": serde_json::to_string(&delta).unwrap_or_default()}),
                    ];
                    (json!({"content": content, "isError": false}), false)
                }
                Ok(ObserveOut::Full(obs, text, image)) => {
                    let mut content = vec![json!({"type": "text", "text": text})];
                    if oreq.mode != ObserveMode::Text {
                        content.push(
                            json!({"type": "text", "text": serde_json::to_string(&obs).unwrap_or_default()}),
                        );
                    }
                    if let Some(png) = image {
                        use base64::Engine;
                        content.push(json!({
                            "type": "image",
                            "data": base64::engine::general_purpose::STANDARD.encode(&png),
                            "mimeType": "image/png"
                        }));
                    }
                    (json!({"content": content, "isError": false}), false)
                }
                Err(e) => (err(e), false),
            }
        }
        "act" => {
            // MCP 调用方不必自 invent request_id（JSON-RPC 的 id 已标识
            // 本次调用）——缺失时由前端补一个唯一值；HTTP 前端保持协议原样。
            let mut args = args;
            if args.get("request_id").is_none() {
                static REQ_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
                let seq = REQ_SEQ.fetch_add(1, Ordering::SeqCst);
                if let Some(obj) = args.as_object_mut() {
                    obj.insert(
                        "request_id".into(),
                        json!(format!("mcp-{}-{seq}", std::process::id())),
                    );
                }
            }
            match serde_json::from_value::<ActionRequest>(args) {
                Ok(req) => {
                    let r = app.ai.act(req).await;
                    let taken = matches!(
                        r,
                        ActionResult::Aborted {
                            reason: AbortReason::UserTakeover
                        }
                    );
                    (
                        json!({
                            "content": [{"type": "text", "text": serde_json::to_string(&r).unwrap_or_default()}],
                            "isError": false
                        }),
                        taken,
                    )
                }
                Err(e) => (err(format!("ActionRequest 解析失败: {e}")), false),
            }
        }
        "screenshot" => match app.ai.marked_screenshot().await {
            Ok(png) => {
                use base64::Engine;
                let b64 = base64::engine::general_purpose::STANDARD.encode(&png);
                (
                    json!({
                        "content": [{"type": "image", "data": b64, "mimeType": "image/png"}],
                        "isError": false
                    }),
                    false,
                )
            }
            Err(e) => (err(e), false),
        },
        _ => (err(format!("未知工具: {name}")), false),
    };
    (result, taken)
}

/// percent-decode（%XX → 字节）；非法序列原样保留。只解 %XX，不把
/// '+' 转空格——观看页用 encodeURIComponent 编码（'+' 会被编成 %2B），
/// 两侧正好往返一致。
fn percent_decode(s: &str) -> String {
    fn hex(b: u8) -> Option<u8> {
        match b {
            b'0'..=b'9' => Some(b - b'0'),
            b'a'..=b'f' => Some(b - b'a' + 10),
            b'A'..=b'F' => Some(b - b'A' + 10),
            _ => None,
        }
    }
    let bytes = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 3 <= bytes.len() {
            if let (Some(h), Some(l)) = (hex(bytes[i + 1]), hex(bytes[i + 2])) {
                out.push(h * 16 + l);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).to_string()
}

#[cfg(test)]
mod tests {
    use super::percent_decode;
    use super::ConnSubs;

    #[test]
    fn percent_decode_round_trips() {
        assert_eq!(percent_decode("abc"), "abc");
        assert_eq!(percent_decode("a%2Bb"), "a+b");
        assert_eq!(percent_decode("a%20b"), "a b");
        assert_eq!(percent_decode("%E4%B8%AD%E6%96%87"), "中文");
        assert_eq!(percent_decode("100%"), "100%");
        assert_eq!(percent_decode("%zz"), "%zz");
        assert_eq!(percent_decode("a+b"), "a+b");
    }

    /// D01：同一连接重复 start/stop 只记一次账——`dom-start,
    /// dom-start,dom-stop,close` 计数恰好回零，不多扣不少补。
    #[test]
    fn conn_subs_dom_is_idempotent_per_connection() {
        let mut s = ConnSubs::new();
        // 自动感知 + 前台显示两次进入：只有第一次真正订阅
        assert!(s.acquire_dom());
        assert!(!s.acquire_dom());
        // 显示退出但感知仍在：由调用方按“是否还有需求”决定；
        // 守卫只保证配对——一次 release 后再次 release 不再扣
        assert!(s.release_dom());
        assert!(!s.release_dom());
        assert!(!s.release_dom());
    }

    /// D02：连接建立即持有 JPEG，释放恰好一次；重复释放不倒扣。
    #[test]
    fn conn_subs_jpeg_released_exactly_once() {
        let mut s = ConnSubs::new();
        assert!(s.release_jpeg());
        assert!(!s.release_jpeg());
    }
}
