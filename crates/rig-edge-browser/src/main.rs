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
mod auth;
mod browser;
mod cdp;
mod changes;
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
use tokio::sync::{watch, Mutex};

use ai::{AiChannel, ObserveOut};
use browser::Browser;
use exec::ExecControl;
use human::HumanChannel;

struct App {
    browser: Arc<Browser>,
    ai: AiChannel,
    human: Arc<HumanChannel>,
    media: Arc<media::MediaPipeline>,
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
    gate_human: watch::Sender<bool>,
    gate_transition: Mutex<()>,
    /// 人通道最近一次启动失败的原因（R-11：状态必须如实——
    /// 启动失败不得把 gate 报成已启用，错误要可查询）
    human_error: std::sync::Mutex<Option<String>>,
    /// 直连鉴权（启动令牌换 access/refresh 票据，见 auth.rs）
    auth: auth::Auth,
    /// 允许内嵌观看页的宿主来源（--embed-origin，逗号分隔）；
    /// 空 = 禁止任何内嵌
    embed_origins: Vec<String>,
}

impl App {
    fn human_enabled(&self) -> bool {
        *self.gate_human.borrow()
    }
}

/// 路由的鉴权类别。AI 路由只认启动令牌；Viewer 路由认 access 票据
/// （或启动令牌，供脚本使用）；未知路径按 Viewer 最严处理。
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
enum Access {
    /// 无秘密的静态资源与换票入口（换票自带令牌校验）
    Public,
    /// 处理函数自行校验：/ws 首帧认证、/stream 路径钥匙
    SelfChecked,
    Viewer,
    Master,
}

fn route_access(path: &str) -> Access {
    match path {
        "/" | "/viewer.js" | "/auth.js" | "/health" | "/ready" | "/session"
        | "/session/refresh" => Access::Public,
        "/ws" => Access::SelfChecked,
        "/observe" | "/act" | "/screenshot.png" => Access::Master,
        p if p.starts_with("/stream/") => Access::SelfChecked,
        _ => Access::Viewer,
    }
}

fn authorized(auth: &auth::Auth, access: Access, bearer: Option<&str>) -> bool {
    match access {
        Access::Public | Access::SelfChecked => true,
        Access::Master => bearer.is_some_and(|t| auth.is_master(t)),
        Access::Viewer => bearer.is_some_and(|t| auth.is_master(t) || auth.check_access(t)),
    }
}

fn bearer_of(headers: &axum::http::HeaderMap) -> Option<&str> {
    headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
}

/// 直连鉴权中间件：令牌只走 `Authorization: Bearer` 头，URL 里不再接受
/// 任何令牌。启动令牌换 access/refresh 票据后，viewer 用 access 调
/// Viewer 路由；AI 路由只认启动令牌（LIU 2026-11-22）。
async fn auth_guard(State(app): State<Arc<App>>, req: Request, next: Next) -> Response {
    let access = route_access(req.uri().path());
    if authorized(&app.auth, access, bearer_of(req.headers())) {
        next.run(req).await
    } else {
        (
            StatusCode::UNAUTHORIZED,
            Json(json!({"error": "需要令牌（Authorization: Bearer）"})),
        )
            .into_response()
    }
}

#[derive(Deserialize)]
struct LoginBody {
    token: String,
}

#[derive(Deserialize)]
struct RefreshBody {
    refresh: String,
}

fn pair_response(app: &App, pair: Option<auth::TokenPair>) -> Response {
    match pair {
        Some(p) => Json(json!({
            "access": p.access,
            "refresh": p.refresh,
            "expires_in": p.expires_in,
            "stream_key": app.auth.stream_key(),
        }))
        .into_response(),
        None => (StatusCode::UNAUTHORIZED, Json(json!({"error": "令牌无效"}))).into_response(),
    }
}

/// 启动令牌（body）换 access + refresh
async fn session_login(State(app): State<Arc<App>>, Json(b): Json<LoginBody>) -> Response {
    pair_response(&app, app.auth.login(&b.token))
}

/// refresh（body）换新一对；旧 refresh 立即作废
async fn session_refresh(State(app): State<Arc<App>>, Json(b): Json<RefreshBody>) -> Response {
    pair_response(&app, app.auth.refresh(&b.refresh))
}

#[cfg(test)]
mod auth_route_tests {
    use super::*;

    #[test]
    fn route_classes() {
        assert_eq!(route_access("/observe"), Access::Master);
        assert_eq!(route_access("/act"), Access::Master);
        assert_eq!(route_access("/screenshot.png"), Access::Master);
        assert_eq!(route_access("/channels"), Access::Viewer);
        assert_eq!(route_access("/print.pdf"), Access::Viewer);
        assert_eq!(route_access("/anything-else"), Access::Viewer);
        assert_eq!(route_access("/ws"), Access::SelfChecked);
        assert_eq!(
            route_access("/stream/k/h264/index.m3u8"),
            Access::SelfChecked
        );
        assert_eq!(route_access("/session"), Access::Public);
        assert_eq!(route_access("/"), Access::Public);
    }

    #[test]
    fn viewer_ticket_cannot_reach_ai_routes() {
        let a = auth::Auth::new("master-secret".into());
        let p = a.login("master-secret").unwrap();
        assert!(authorized(&a, Access::Viewer, Some(&p.access)));
        assert!(authorized(&a, Access::Viewer, Some("master-secret")));
        assert!(!authorized(&a, Access::Master, Some(&p.access)));
        assert!(authorized(&a, Access::Master, Some("master-secret")));
        assert!(!authorized(&a, Access::Viewer, None));
        assert!(!authorized(&a, Access::Master, Some(&p.refresh)));
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
    /// 是否允许 HLS 播放。管线有有效播放请求时运行。
    av_wanted: bool,
    /// M03 HLS 运行态：running / idle / failed（供排障，不抛异常）
    media_state: String,
    /// M03 HLS 最近一次启动失败原因；正常时为 null
    media_error: Option<String>,
    /// HLS 输出版本表（默认 ["hevc","h264"]；RIG_HLS_SINGLE 只留一路）。
    /// 观看端按此表与自身解码能力选流
    hls_renditions: Vec<String>,
}

async fn channel_state(app: &Arc<App>, av: bool) -> ChannelState {
    // M03：wanted/running/error 一次取齐，避免并发下“running
    // 与 error 对不上”
    let (wanted, running, media_error) = app.media.state_summary().await;
    let media_state = if running {
        "running"
    } else if media_error.is_some() {
        "failed"
    } else {
        "idle"
    }
    .to_string();
    ChannelState {
        ai: app.gate_ai.load(Ordering::SeqCst),
        human: app.human_enabled(),
        av,
        av_available: app.av_capable,
        human_error: app.human_error.lock().unwrap().clone(),
        av_wanted: wanted,
        media_state,
        media_error,
        hls_renditions: media::offered_renditions()
            .iter()
            .map(|s| s.to_string())
            .collect(),
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
    let token = get_arg("--token")
        .or_else(|| std::env::var("RIG_EDGE_TOKEN").ok())
        .filter(|t| !t.is_empty());
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
    // 直连安全底线（LIU 2026-11-22）：只要对外开 HTTP（纯 HTTP 模式或
    // stdio+viewer），令牌必设——由启动程序随机生成并传入；绑出回环
    // 令牌与票据靠 HTTPS 保密：非回环监听由 build_tls 强制 TLS。
    let serves_http = !has("--stdio") || human_default;
    if serves_http && token.is_none() {
        eprintln!(
            "拒绝启动：对外提供 HTTP 必须设令牌。请由启动程序随机生成并传入 --token <令牌>（或环境变量 RIG_EDGE_TOKEN）。"
        );
        std::process::exit(2);
    }
    let embed_origins: Vec<String> = get_arg("--embed-origin")
        .map(|v| {
            v.split(',')
                .map(|o| o.trim().trim_end_matches('/').to_string())
                .filter(|o| !o.is_empty())
                .collect()
        })
        .unwrap_or_default();

    // 虚拟声卡先行：Chromium 出生时就把声音放进 null sink，
    // 之后 ffmpeg 从它的 monitor 合流（LIU 2026-11-18 定的音频方案）。
    let av_capable = media::ensure_virtual_sink().await;
    if av_capable {
        std::env::set_var("PULSE_SINK", media::SINK_NAME);
    }

    // Upload 只允许取自该目录（人要给 AI 传文件，把文件放进来）：给了
    // --cache-dir 用其下 uploads/；没给则用本进程的一次性临时目录
    let upload_root = match cache_dir.as_deref() {
        Some(d) => std::path::Path::new(d).join("uploads"),
        None => std::env::temp_dir().join(format!("rig-edge-{}-uploads", std::process::id())),
    };
    std::fs::create_dir_all(&upload_root).expect("创建上传目录失败");
    // 没有专门指定的目录都是本进程的一次性临时目录：退出时清理，
    // 不在磁盘上留登录态/下载的文件（SIGKILL 无法兜底）
    let mut temp_dirs: Vec<std::path::PathBuf> = Vec::new();
    if cache_dir.is_none() {
        temp_dirs.push(upload_root.clone());
    }
    if profile_dir.is_none() && get_arg("--cdp-url").is_none() {
        temp_dirs.push(browser::temp_profile_dir());
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
    eprintln!("rig-edge-browser: uploads={}", upload_root.display());
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
        ai: AiChannel::new(browser.clone(), origins, exec.clone())
            .with_upload_root(&upload_root)
            .expect("上传目录不可用"),
        exec,
        media: Arc::new(media::MediaPipeline::new(human.clone())),
        rtc: rtc::RtcHub::new(human.clone(), stun),
        dom: dom::DomChannel::new(browser.clone(), human.clone()),
        listen_ip,
        human,
        browser,
        av_capable,
        gate_ai: AtomicBool::new(ai_default),
        gate_human: watch::channel(human_default).0,
        gate_transition: Mutex::new(()),
        human_error: std::sync::Mutex::new(None),
        auth: auth::Auth::new(token.unwrap_or_default()),
        embed_origins,
    });
    app.human.set_gate_display(ai_default, human_default).await;
    app.human.manage_capture();
    if human_default {
        if let Err(e) = app.human.start().await {
            // R-11：启动失败如实降级——gate 不报已启用，错误可查
            eprintln!("人通道启动失败: {e}");
            app.gate_human.send_replace(false);
            *app.human_error.lock().unwrap() = Some(e);
            app.human.set_gate_display(ai_default, false).await;
        }
        if av_default && av_capable && app.human_enabled() {
            app.media.apply_wanted(true).await;
        }
    }

    // 路由拆分：Viewer 路由（观看页/换票/WS/媒体）与 AI 路由（HTTP
    // 形态的 observe/act）。stdio 模式只挂 Viewer 路由，AI 只能走 MCP，
    // 本机其他进程无法经 HTTP 绕过 AI 闸门。
    let viewer_router = Router::new()
        .route("/", get(viewer))
        .route("/viewer.js", get(viewer_js))
        .route("/auth.js", get(auth_js))
        .route("/health", get(health))
        .route("/ready", get(ready))
        .route("/session", post(session_login))
        .route("/session/refresh", post(session_refresh))
        .route("/channels", get(get_channels).post(set_channels))
        .route("/print.pdf", get(print_pdf))
        .route("/stream/{key}/{rend}/{name}", get(stream_file))
        .route("/ws", get(ws_handler));
    let ai_router = Router::new()
        .route("/observe", post(observe))
        .route("/act", post(act))
        .route("/screenshot.png", get(screenshot));
    let router = if has("--stdio") {
        viewer_router
    } else {
        viewer_router.merge(ai_router)
    }
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
            let temp_sig = temp_dirs.clone();
            tokio::spawn(async move {
                let mut term =
                    tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                        .expect("注册 SIGTERM 处理失败");
                tokio::select! {
                    _ = tokio::signal::ctrl_c() => {}
                    _ = term.recv() => {}
                }
                app_sig.browser.shutdown().await;
                remove_temp_dirs(&temp_sig);
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
    remove_temp_dirs(&temp_dirs);
}

/// 清理本进程的一次性临时目录（须在 Chromium 退出之后调用）
fn remove_temp_dirs(dirs: &[std::path::PathBuf]) {
    for d in dirs {
        let _ = std::fs::remove_dir_all(d);
    }
}

async fn viewer(State(app): State<Arc<App>>) -> impl axum::response::IntoResponse {
    // 只有 --embed-origin 白名单里的宿主能内嵌观看页；未配置即禁止
    let ancestors = if app.embed_origins.is_empty() {
        "'none'".to_string()
    } else {
        app.embed_origins.join(" ")
    };
    /* no-store：观看页随边缘版本演进，浏览器缓存住旧版页面/
    脚本会让已修的状态机在用户侧继续跑旧逻辑（且无任何
    提示）——每次加载都取当前构建的版本 */
    (
        [
            (axum::http::header::CACHE_CONTROL, "no-store".to_string()),
            (
                axum::http::header::CONTENT_SECURITY_POLICY,
                format!("frame-ancestors {ancestors}"),
            ),
        ],
        Html(VIEWER_HTML),
    )
}

async fn auth_js() -> impl axum::response::IntoResponse {
    (
        [
            (
                axum::http::header::CONTENT_TYPE,
                "text/javascript; charset=utf-8",
            ),
            (axum::http::header::CACHE_CONTROL, "no-store"),
        ],
        AUTH_JS,
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
        "human": app.human_enabled(),
        "human_running": app.human.is_running(),
        "frame_size": [fw, fh],
        "sessions": sessions,
        "cdp_connected": cdp_connected,
        "last_frame_age_ms": app.human.last_frame_age_ms(),
        "exec_in_flight": app.ai.exec_in_flight().await,
        "rtc_connected": app.rtc.connected_count(),
        "rtc_video_want": app.rtc.video_want_count(),
        "rtc_audio_want": app.rtc.audio_want_count(),
        "dc_pending_bytes": app.rtc.dc_pending_bytes(),
        "dom_viewers": app.dom.viewer_count(),
        "media": {
            "wanted": app.media.wanted(),
            "running": app.media.running().await,
            "error": app.media.state_summary().await.2,
            "renditions": media::offered_renditions(),
        },
        "pump": {
            "full": m.pump_full.load(Ordering::Relaxed),
            "content": m.pump_content.load(Ordering::Relaxed),
            "pasted": m.pump_pasted.load(Ordering::Relaxed),
            "dropped": m.pump_dropped.load(Ordering::Relaxed),
            "rect_refetch": m.pump_rect_refetch.load(Ordering::Relaxed),
        },
        "changes": {
            "seq": app.ai.changes_mark(),
            "observe_unchanged": m.observe_unchanged.load(Ordering::Relaxed),
        },
        "act_precheck": {
            "fast_hit": m.act_fast_hit.load(Ordering::Relaxed),
            "fast_miss": m.act_fast_miss.load(Ordering::Relaxed),
        },
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
    Json(channel_state(&app, av).await)
}

/// 应用通道开关（HTTP /channels 与观看端菜单共用同一逻辑），
/// 并同步观看端菜单里的勾选态显示。
async fn apply_gate_flags(app: &Arc<App>, ai: Option<bool>, human: Option<bool>, av: Option<bool>) {
    let _transition = app.gate_transition.lock().await;
    if let Some(h) = human {
        let cur = app.human_enabled();
        if h && !cur {
            // R-11：start 成功才置 gate；失败保留关闭并记录原因
            match app.human.start().await {
                Ok(()) => {
                    *app.human_error.lock().unwrap() = None;
                    app.gate_human.send_replace(true);
                }
                Err(e) => {
                    eprintln!("人通道启动失败: {e}");
                    *app.human_error.lock().unwrap() = Some(e);
                }
            }
        } else if !h {
            // 先通知旧连接停收发，再停止共享画面与 HLS 管线。
            if cur {
                app.gate_human.send_replace(false);
            }
            let _ = app.human.stop().await;
            app.media.apply_wanted(false).await;
        }
    }
    if let Some(a) = ai {
        app.gate_ai.store(a, Ordering::SeqCst);
        if !a {
            // 关闭 AI 通道 = 撤销当前执行代次：在途 act 在下一个检查点中止，
            // 重开后须先 observe 重新同步才能 act（与强制接管同一机制）
            app.exec.request_abort().await;
        }
    }
    if let Some(av) = av {
        if !av || app.av_capable && app.human_enabled() {
            app.media.apply_wanted(av).await;
        }
    }
    app.human
        .set_gate_display(app.gate_ai.load(Ordering::SeqCst), app.human_enabled())
        .await;
}

async fn set_channels(
    State(app): State<Arc<App>>,
    Json(set): Json<ChannelSet>,
) -> Json<ChannelState> {
    apply_gate_flags(&app, set.ai, set.human, set.av).await;
    let av = app.media.running().await;
    Json(channel_state(&app, av).await)
}

/// 观看端「打印」菜单的产物取回（打印一次取一次，取后清空）
async fn print_pdf(State(app): State<Arc<App>>) -> Response {
    if !app.human_enabled() {
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
    axum::extract::Path((key, rend, name)): axum::extract::Path<(String, String, String)>,
) -> Response {
    // 媒体元素无法带请求头：路径钥匙只授权只读的 /stream，
    // 且仅认证过的 viewer 能拿到；播放列表里的相对段地址继承该前缀
    if !app.auth.check_stream_key(&key) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    if !app.human_enabled() {
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
    // 被拒（页面状态不符）时附带最新一帧，调用方不必再补一次 observe
    let (result, fresh) = app.ai.act_with_fresh(req).await;
    match fresh {
        Ok(Some(ObserveOut::Delta(delta, text))) => {
            Json(json!({"result": result, "delta": delta, "text": text})).into_response()
        }
        Ok(Some(ObserveOut::Full(obs, text, _))) => {
            Json(json!({"result": result, "observation": obs, "text": text})).into_response()
        }
        Err(text) => Json(json!({"result": result, "text": text})).into_response(),
        Ok(None) => Json(json!({"result": result})).into_response(),
    }
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
    headers: axum::http::HeaderMap,
    ws: WebSocketUpgrade,
    axum::extract::ConnectInfo(remote): axum::extract::ConnectInfo<std::net::SocketAddr>,
) -> Response {
    // 升级期间若发生关开，已获准的旧连接也必须退出。
    let gate = app.gate_human.subscribe();
    if !*gate.borrow() {
        return channel_off();
    }
    // 非浏览器客户端可在升级请求里带 Bearer；浏览器 WebSocket 无法设头，
    // 升级后须在 3 秒内以首帧 {"type":"auth","access":…} 认证
    let pre_authed = authorized(&app.auth, Access::Viewer, bearer_of(&headers));
    ws.on_upgrade(move |socket| ws_session(app, socket, remote, gate, pre_authed))
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

/// 本连接是否显示 DOM，同时决定 WS 与 RTC 的镜像路由。
#[derive(Debug, Default)]
struct ConnSubs {
    dom_held: Arc<AtomicBool>,
}

impl ConnSubs {
    fn acquire_dom(&self) -> bool {
        !self.dom_held.swap(true, Ordering::SeqCst)
    }

    fn release_dom(&self) -> bool {
        self.dom_held.swap(false, Ordering::SeqCst)
    }
}

fn rtc_tracks(v: &serde_json::Value) -> rtc::TrackDemand {
    let wanted = |key| {
        v.get(key)
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(true)
    };
    rtc::TrackDemand {
        video: wanted("video"),
        audio: wanted("audio"),
    }
}

async fn first_viewer_frame(
    app: &App,
    frames: &mut tokio::sync::broadcast::Receiver<(browser::ActivePage, human::FrameMsg)>,
) -> Option<human::FrameMsg> {
    let first = tokio::time::timeout(std::time::Duration::from_millis(150), async {
        loop {
            match frames.recv().await {
                Ok((page, frame)) if app.browser.active_page().await.as_ref() == Ok(&page) => {
                    return Some(frame);
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => return None,
                _ => {}
            }
        }
    })
    .await;
    if let Ok(Some(frame)) = first {
        return Some(frame);
    }
    let (page, frame) =
        tokio::time::timeout(std::time::Duration::from_secs(1), app.human.first_frame())
            .await
            .ok()??;
    *frames = frames.resubscribe();
    (app.browser.active_page().await.as_ref() == Ok(&page)).then_some(frame)
}

/// 首帧认证：3 秒内收到合法的 auth 帧才放行，其他一律拒绝
async fn ws_first_frame_auth(
    auth: &auth::Auth,
    stream: &mut futures_util::stream::SplitStream<WebSocket>,
) -> bool {
    let wait = async {
        while let Some(Ok(m)) = stream.next().await {
            if let Message::Text(t) = m {
                let v: serde_json::Value = serde_json::from_str(&t).unwrap_or_default();
                let ok = v.get("type").and_then(serde_json::Value::as_str) == Some("auth")
                    && v.get("access")
                        .and_then(serde_json::Value::as_str)
                        .is_some_and(|a| authorized(auth, Access::Viewer, Some(a)));
                return ok;
            }
        }
        false
    };
    tokio::time::timeout(std::time::Duration::from_secs(3), wait)
        .await
        .unwrap_or(false)
}

async fn ws_session(
    app: Arc<App>,
    socket: WebSocket,
    remote: std::net::SocketAddr,
    mut gate: watch::Receiver<bool>,
    pre_authed: bool,
) {
    if gate.has_changed().unwrap_or(true) {
        return;
    }
    let (mut sink, mut stream) = socket.split();
    if !pre_authed && !ws_first_frame_auth(&app.auth, &mut stream).await {
        let _ = sink.send(Message::Close(None)).await;
        return;
    }
    // 订阅先于声明需求，让首帧可以直接接收。
    let mut frames = app.human.subscribe();
    let mut jpeg_lease = Some(app.human.request_capture());
    let (jpeg_mode, mut jpeg_watch) = tokio::sync::watch::channel(true);
    let mut notices = app.human.subscribe_notices();
    let mut dom_msgs = app.dom.subscribe();
    let app2 = app.clone();
    let send_app = app.clone();
    // D01/D02：本连接的订阅记账（跨两个任务共享，释放只在父作用域）。
    let subs = Arc::new(ConnSubs::default());
    let dom_wanted = subs.dom_held.clone();
    let subs2 = subs.clone();
    // 本连接的 transport 模式：RTC 连通后置 true，发送侧停发 WS 帧
    // （广播照常接收只丢弃）；RTC 断开/关闭自动恢复 WS 帧
    let rtc_active = Arc::new(AtomicBool::new(false));
    let rtc_on = rtc_active.clone();
    // 本连接的 rig-dom 数据通道已打开时，DOM 消息改走 DC：
    // 发送侧跳过 WS 文本副本（会话任务用自己的订阅经 DC 送达）
    let dom_dc_open = Arc::new(AtomicBool::new(false));
    let dom_dc_on = dom_dc_open.clone();
    // 额外的出站消息（如剪贴板读回）与帧流共用一个发送端
    let (out_tx, mut out_rx) = tokio::sync::mpsc::channel::<String>(16);
    let exec_ghost = app.exec.clone();
    let human2 = app.human.clone();
    let mut send_task = tokio::spawn(async move {
        // hello：告知 CSS 视口尺寸，关闭通道时可中断这次读取。
        if let Ok(main) = send_app.browser.main().await {
            if let Ok((w, h, _)) = send_app.browser.layout(&main).await {
                let hello = json!({"type": "hello", "cssWidth": w, "cssHeight": h});
                if sink
                    .send(Message::Text(hello.to_string().into()))
                    .await
                    .is_err()
                {
                    return;
                }
            }
        }
        if let Some(f) = first_viewer_frame(&send_app, &mut frames).await {
            let wanted = *jpeg_watch.borrow();
            if wanted && sink.send(frame_message(&f)).await.is_err() {
                return;
            }
        }
        // 页面首帧后直发工具栏初值，后续由通知广播更新。
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
                changed = jpeg_watch.changed() => {
                    if changed.is_err() { break; }
                    let wanted = *jpeg_watch.borrow_and_update();
                    if wanted && !rtc_on.load(Ordering::SeqCst) {
                        frames = frames.resubscribe();
                        if let Some(f) = first_viewer_frame(&send_app, &mut frames).await {
                            let wanted = *jpeg_watch.borrow();
                            if wanted && sink.send(frame_message(&f)).await.is_err() {
                                break;
                            }
                        }
                    }
                },
                frame = frames.recv() => match frame {
                    Ok((page, f)) => {
                        let wanted = *jpeg_watch.borrow();
                        if wanted && !rtc_on.load(Ordering::SeqCst)
                            && send_app.browser.active_page().await.as_ref() == Ok(&page)
                            && sink.send(frame_message(&f)).await.is_err()
                        {
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
                        if !dom_wanted.load(Ordering::SeqCst) || dom_dc_on.load(Ordering::SeqCst) {
                            continue;
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
    let (send_done, mut stop_recv) = tokio::sync::oneshot::channel::<()>();
    let recv_gate = gate.clone();
    let mut recv_task = tokio::spawn(async move {
        let mut pointer = human::ViewerPointer::default();
        let mut keys = human::ViewerKeys::default();
        let mut rtc_session: Option<rtc::SessionHandle> = None;
        // R-10：收包按帧型分发——Ping/Pong/Binary 不结束会话
        // （此前 while let 只认 Text，任意保活帧都会拆连接）
        loop {
            let msg = tokio::select! {
                biased;
                _ = &mut stop_recv => break,
                msg = stream.next() => msg,
            };
            let Some(msg) = msg else { break };
            if recv_gate.has_changed().unwrap_or(true) {
                break;
            }
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
                Some("mouse" | "wheel") => app2.human.handle_pointer(&mut pointer, &v).await,
                Some("key") => app2.human.handle_key(&mut keys, &v).await,
                Some("input-reset") => {
                    app2.human.release_pointer(&mut pointer).await;
                    app2.human.release_keys(&mut keys).await;
                }
                Some("jpeg-demand") => {
                    let on = v.get("on").and_then(|b| b.as_bool()).unwrap_or(true);
                    if on && jpeg_lease.is_none() {
                        jpeg_lease = Some(app2.human.request_capture());
                        jpeg_mode.send_replace(true);
                    } else if !on && jpeg_lease.is_some() {
                        jpeg_lease = None;
                        jpeg_mode.send_replace(false);
                    }
                }
                Some("rtc-start") => {
                    rtc_session.take(); // drop 旧会话（其驱动任务随之退出）
                    let ip = candidate_ip(&app2, remote);
                    rtc_session = Some(app2.rtc.start_session(
                        out_tx.clone(),
                        ip,
                        app2.dom.subscribe(),
                        rtc::ViewerTransport {
                            video_active: rtc_active.clone(),
                            dom_open: dom_dc_open.clone(),
                            dom_wanted: subs2.dom_held.clone(),
                            gate: recv_gate.clone(),
                        },
                        rtc_tracks(&v),
                    ));
                }
                Some("rtc-demand") => {
                    if let Some(s) = &rtc_session {
                        s.set_demand(rtc_tracks(&v)).await;
                    }
                }
                Some("mode-summary") => {
                    if let Some(request) = v.get("request").and_then(|v| v.as_u64()) {
                        if let Some(mut summary) = app2.dom.mode_summary().await {
                            summary["type"] = json!("mode-summary");
                            summary["request"] = json!(request);
                            let _ = out_tx.send(summary.to_string()).await;
                        }
                    }
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
                    if subs2.acquire_dom() {
                        app2.dom.viewer_start().await;
                    }
                }
                Some("dom-stop") => {
                    if subs2.release_dom() {
                        app2.dom.viewer_stop().await;
                    }
                }
                Some("dom-resync") => {
                    if subs2.dom_held.load(Ordering::SeqCst) {
                        app2.dom.resync().await;
                    }
                }
                _ => {
                    // DOM 模式的输入带 dom:1，坐标是页面坐标，走直达分发
                    if v.get("dom").and_then(|x| x.as_bool()) == Some(true) {
                        app2.human.handle_ws_dom(&v, &out_tx).await;
                        continue;
                    }
                    // 输入分发全在人通道（chrome 命中/编辑器/遮蔽接管）；
                    // 菜单里的通道开关以返回值带回这里应用 gate
                    if let Some((a, h)) = app2.human.handle_ws(&v, &out_tx).await {
                        apply_gate_flags(&app2, Some(a), Some(h), None).await;
                    }
                }
            }
        }
        drop(rtc_session);
        app2.human.release_pointer(&mut pointer).await;
        app2.human.release_keys(&mut keys).await;
    });
    tokio::select! {
        _ = gate.changed() => {
            let _ = send_done.send(());
            send_task.abort();
            let _ = send_task.await;
            let _ = recv_task.await;
        }
        _ = &mut send_task => {
            // Let any in-flight subscription transition finish before cleanup.
            let _ = send_done.send(());
            let _ = recv_task.await;
        }
        _ = &mut recv_task => {
            send_task.abort();
            let _ = send_task.await;
        }
    }
    if subs.release_dom() {
        app.dom.viewer_stop().await;
    }
}

const VIEWER_HTML: &str = include_str!("viewer.html");
const AUTH_JS: &str = include_str!("auth.js");
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

/// 本实现只用 tools 子集（三个版本线格式一致）；客户端请求的版本在
/// 列表内就回同一版本，否则回我们的最新版，由客户端决定是否继续。
const MCP_VERSIONS: [&str; 3] = ["2025-06-18", "2025-03-26", "2024-11-05"];

fn negotiate_mcp_version(requested: Option<&str>) -> &'static str {
    MCP_VERSIONS
        .iter()
        .find(|v| Some(**v) == requested)
        .copied()
        .unwrap_or(MCP_VERSIONS[0])
}

/// 返回 (结果, 是否发生了用户强制接管)
async fn mcp_dispatch(
    app: &Arc<App>,
    method: &str,
    params: &serde_json::Value,
) -> Result<(serde_json::Value, bool), (i64, String)> {
    match method {
        "initialize" => {
            let v = negotiate_mcp_version(
                params
                    .get("protocolVersion")
                    .and_then(serde_json::Value::as_str),
            );
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
            "description": "观察当前页面：返回紧凑文本渲染与完整语义快照（Observation JSON）。动手前先观察，动作引用快照里的元素 id 与 frame_id。可选参数（ObserveRequest）：mode='text' 只回文本视图（最省）、'tree' 树+文本（默认）、'tree_image' 再附编号截图；budget_tokens 按预算裁剪响应视图（被裁元素仍可凭 id 操作）；subtree 只回某元素的子树；include_offscreen=true 才带视口外元素（默认不带）；since_frame 使用客户端最后收到并应用的观察帧号。页面、坐标空间与投影相容时回增量（摘要和 ObservationDelta JSON），否则回完整快照。",
            "inputSchema": {"type": "object", "properties": {"mode": {"type": "string", "enum": ["text", "tree", "tree_image"]}, "budget_tokens": {"type": "integer", "minimum": 0}, "subtree": {"type": "string"}, "since_frame": {"type": "integer", "minimum": 0}, "include_offscreen": {"type": "boolean"}}, "additionalProperties": false}
        },
        {
            "name": "act",
            "description": "执行一个动作。参数为 ActionRequest：{frame_id, action}。action 示例：{kind:'click', target:{by:'element', id:'e3'}}、{kind:'type', target:{by:'element', id:'e3'}, text:'内容', submit:false}、{kind:'key', key:'Control+A'}、{kind:'navigate', url:'https://…'}、{kind:'scroll', dx:0, dy:600}、{kind:'upload', target:{by:'element', id:'e3'}, files:['a.pdf']}（给文件框喂文件；只能取自边缘的上传目录，相对名按该目录解析，目录外一律拒绝）。frame_id 用最近一次 observe 的帧号，页面变化后陈旧帧会被拒（重新 observe 即可）。",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "frame_id": {"type": "integer"},
                    "action": {"type": "object"},
                    "request_id": {"type": "string", "description": "可选；用于幂等与追踪，缺省时边缘自动生成"}
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
                    let (r, fresh) = app.ai.act_with_fresh(req).await;
                    let taken = matches!(
                        r,
                        ActionResult::Aborted {
                            reason: AbortReason::UserTakeover
                        }
                    );
                    let mut content = vec![
                        json!({"type": "text", "text": serde_json::to_string(&r).unwrap_or_default()}),
                    ];
                    if taken {
                        // 契约里的 Aborted 不变；停止指示以附加文本块给到智能体
                        content.push(json!({"type": "text", "text": "用户已强制接管浏览器：请停止原任务，不要重试已中断的动作，等待用户的新指示后再重新 observe。"}));
                    }
                    // 被拒时附最新帧文本视图（第二个文本块）
                    match fresh {
                        Ok(Some(ObserveOut::Delta(delta, text))) => {
                            content.push(json!({"type": "text", "text": text}));
                            content.push(
                                json!({"type": "text", "text": serde_json::to_string(&delta).unwrap_or_default()}),
                            );
                        }
                        Ok(Some(ObserveOut::Full(obs, text, _))) => {
                            content.push(json!({"type": "text", "text": text}));
                            content.push(
                                json!({"type": "text", "text": serde_json::to_string(&obs).unwrap_or_default()}),
                            );
                        }
                        Err(text) => content.push(json!({"type": "text", "text": text})),
                        Ok(None) => {}
                    }
                    (json!({"content": content, "isError": false}), taken)
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

#[cfg(test)]
mod tests {
    #[test]
    fn remove_temp_dirs_deletes_contents_and_tolerates_missing() {
        let d = std::env::temp_dir().join(format!("rig-cleanup-ut-{}", std::process::id()));
        std::fs::create_dir_all(d.join("sub")).unwrap();
        std::fs::write(d.join("sub/f.txt"), b"x").unwrap();
        let missing = d.join("nope");
        super::remove_temp_dirs(&[d.clone(), missing]);
        assert!(!d.exists());
    }

    #[test]
    fn mcp_version_negotiation_never_echoes_unknown() {
        assert_eq!(
            super::negotiate_mcp_version(Some("2025-03-26")),
            "2025-03-26"
        );
        assert_eq!(
            super::negotiate_mcp_version(Some("9999-01-01")),
            "2025-06-18"
        );
        assert_eq!(super::negotiate_mcp_version(None), "2025-06-18");
    }

    use super::ConnSubs;

    /// D01：同一连接重复 start/stop 只记一次账——`dom-start,
    /// dom-start,dom-stop,close` 计数恰好回零，不多扣不少补。
    #[test]
    fn conn_subs_dom_is_idempotent_per_connection() {
        let s = ConnSubs::default();
        assert!(s.acquire_dom());
        assert!(!s.acquire_dom());
        assert!(s.release_dom());
        assert!(!s.release_dom());
        assert!(!s.release_dom());
    }
}
