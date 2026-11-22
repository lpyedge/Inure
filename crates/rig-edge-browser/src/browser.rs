//! Chromium 托管与页面门面：启动无头 Chromium、挂载全部目标会话
//! （含跨进程 iframe），并把快照/动作/截圖/投屏所需的 CDP 调用收敛在这里。

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::Mutex;

use crate::cdp::{Cdp, CdpResult};

/// DOM 节点信息：(标签, 属性, 所属 frameId)
pub type DomInfo = (String, HashMap<String, String>, Option<String>);
/// backendNodeId → DOM 节点信息
pub type DomMap = HashMap<i64, DomInfo>;

#[derive(Clone, Debug)]
pub struct SessionInfo {
    pub target_id: String,
    pub target_type: String,
    /// 挂靠事件所在的父会话：浏览器级挂靠（顶层页面）为 None，
    /// OOPIF 挂在页面会话下为 Some(页面会话 id)。快照据此圈定
    /// 当前页面的帧树范围，其他标签的会话不混入（§7/B4）。
    pub parent: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ActivePage {
    pub session: String,
    generation: u64,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ViewState {
    pub space: rig_proto::observation::Space,
    pub page_origin: [f32; 2],
    pub visual_offset: [f32; 2],
    pub visual_scale: f32,
}

impl ViewState {
    pub fn same_space(&self, other: &Self) -> bool {
        self.space == other.space
            && self.visual_offset == other.visual_offset
            && self.visual_scale == other.visual_scale
    }
}

pub struct Browser {
    pub cdp: Arc<Cdp>,
    child: Mutex<Option<tokio::process::Child>>,
    pub sessions: Mutex<HashMap<String, SessionInfo>>,
    main_session: Mutex<(Option<String>, u64)>,
    /// 规范化后的 UA（去掉 Headless 字样，版本取自浏览器自身）：
    /// 每个页面会话挂载时以 Emulation.setUserAgentOverride 套用。
    /// 带 Headless 的 UA 是最廉价的机器人信号，多家站点据此拒绝
    /// 播放器初始化（YouTube 实测）。RIG_EDGE_UA 可整串覆盖。
    ua: Mutex<Option<String>>,
    /// 与 ua 配套的 UA 客户端提示元数据（Emulation 的
    /// userAgentMetadata）。只改 UA 串不带元数据时，Chromium 会把
    /// navigator.userAgentData 的高熵值整体清空——空元数据本身
    /// 就是异常信号（真 Chrome 恒有 brands/platform/完整版本）。
    ua_meta: Mutex<Option<Value>>,
    /// 覆盖的 Accept-Language（setUserAgentOverride 的
    /// acceptLanguage 参数，同时驱动 navigator.languages）。
    /// 启动参数 --lang 在无头模式下不驱动这两个值（实测），
    /// 必须经覆盖下发；RIG_EDGE_LANG 决定内容。
    accept_lang: Mutex<Option<String>>,
    /// 人格时区（Emulation.setTimezoneOverride）：仅自拉浏览器
    /// 套用，挂靠用户自己的浏览器不动。RIG_EDGE_TZ 覆盖，
    /// 默认 Asia/Tokyo（LIU 本人的人格一致性：语言/时区/账号史
    /// 都在日本；出口 IP 地理属部署问题，不在此层）。
    persona_tz: Option<String>,
}

/// 仅在 Linux 读取实际 UID；无法读取时保留 Chromium 沙箱。
#[cfg(target_os = "linux")]
fn running_as_root() -> bool {
    std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|text| {
            text.lines()
                .find(|l| l.starts_with("Uid:"))
                .and_then(|l| l.split_whitespace().nth(1).map(|u| u == "0"))
        })
        .unwrap_or(false)
}

#[cfg(not(target_os = "linux"))]
fn running_as_root() -> bool {
    false
}

/// 调试输出用的 URL 脱敏：只留 scheme://host/path，去掉 query 与 fragment
/// （query 里可能有会话密钥，GPT review R-11）；userinfo 一并去掉
/// （`https://user:secret@host` 的凭证不得进日志，整改第二期 R-22）。
fn url_for_log(url: &str) -> String {
    let cut = url.find(['?', '#']).unwrap_or(url.len());
    let base = &url[..cut];
    // userinfo 位于 scheme:// 之后、最后一个 @ 之前
    if let Some(scheme_end) = base.find("://") {
        let rest = &base[scheme_end + 3..];
        if let Some(at) = rest.rfind('@') {
            return format!("{}://{}", &base[..scheme_end], &rest[at + 1..]);
        }
    }
    base.to_string()
}

fn find_chromium(explicit: Option<&str>) -> CdpResult<PathBuf> {
    let bundled = std::env::var_os("RIG_EDGE_BUNDLED_CHROMIUM_DIR").map(PathBuf::from);
    let bundled = bundled.as_deref();
    let path = std::env::var_os("PATH");
    let installed_dirs = installed_chromium_dirs();
    resolve_chromium(
        explicit,
        bundled,
        path.as_deref(),
        &installed_dirs,
        path_chromium_names(),
    )
}

fn chromium_in_dir(dir: &std::path::Path) -> Option<PathBuf> {
    #[cfg(windows)]
    let names = ["chrome.exe", "chromium.exe"];
    #[cfg(not(windows))]
    let names = ["chrome", "chromium"];
    let executable = names
        .iter()
        .map(|name| dir.join(name))
        .find(|path| is_executable(path));
    #[cfg(target_os = "macos")]
    return executable.or_else(|| mac_chromium_bundle_paths(dir).find(|path| is_executable(path)));
    #[cfg(not(target_os = "macos"))]
    executable
}

#[cfg(target_os = "macos")]
fn mac_chromium_bundle_paths(dir: &std::path::Path) -> impl Iterator<Item = PathBuf> {
    let mut paths: Vec<PathBuf> = [
        "Google Chrome for Testing.app/Contents/MacOS/Google Chrome for Testing",
        "Google Chrome.app/Contents/MacOS/Google Chrome",
        "Chromium.app/Contents/MacOS/Chromium",
    ]
    .iter()
    .map(|relative| dir.join(relative))
    .collect();
    paths.extend([
        dir.join("Contents/MacOS/Google Chrome for Testing"),
        dir.join("Contents/MacOS/Google Chrome"),
        dir.join("Contents/MacOS/Chromium"),
    ]);
    paths.into_iter()
}

fn path_chromium_names() -> &'static [&'static str] {
    #[cfg(windows)]
    {
        &["chrome.exe", "chromium.exe"]
    }
    #[cfg(not(windows))]
    {
        &[
            "chromium",
            "chromium-browser",
            "google-chrome",
            "google-chrome-stable",
        ]
    }
}

fn installed_chromium_dirs() -> Vec<PathBuf> {
    #[allow(unused_mut)]
    let mut dirs = Vec::new();
    #[cfg(windows)]
    {
        for variable in ["ProgramFiles", "ProgramFiles(x86)", "LOCALAPPDATA"] {
            if let Some(root) = std::env::var_os(variable).map(PathBuf::from) {
                dirs.push(root.join("Google/Chrome/Application"));
            }
        }
    }
    #[cfg(target_os = "macos")]
    {
        for root in [
            Some(PathBuf::from("/Applications")),
            std::env::var_os("HOME").map(|home| PathBuf::from(home).join("Applications")),
        ]
        .into_iter()
        .flatten()
        {
            dirs.push(root);
        }
    }
    dirs
}

fn resolve_chromium(
    explicit: Option<&str>,
    bundled: Option<&std::path::Path>,
    path: Option<&std::ffi::OsStr>,
    installed_dirs: &[PathBuf],
    path_names: &[&str],
) -> CdpResult<PathBuf> {
    if let Some(dir) = bundled.filter(|dir| dir.is_absolute()) {
        if let Some(path) = chromium_in_dir(dir) {
            return Ok(path);
        }
    }
    for name in path_names {
        if let Some(path) = find_on_path(name, path) {
            return Ok(path);
        }
    }
    for dir in installed_dirs {
        if let Some(path) = chromium_in_dir(dir) {
            return Ok(path);
        }
    }
    if let Some(dir) = explicit {
        if let Some(path) = chromium_in_dir(std::path::Path::new(dir)) {
            return Ok(path);
        }
        return Err(format!(
            "No executable Chromium found in --chromium-dir {dir}"
        ));
    }
    Err(
        "Chromium not found beside the release or in system locations; specify --chromium-dir"
            .into(),
    )
}

fn find_on_path(name: &str, path: Option<&std::ffi::OsStr>) -> Option<PathBuf> {
    std::env::split_paths(path?)
        .map(|dir| dir.join(name))
        .find(|candidate| is_executable(candidate))
}

fn is_executable(path: &std::path::Path) -> bool {
    let Ok(metadata) = std::fs::metadata(path) else {
        return false;
    };
    if !metadata.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

async fn http_get_json(url: &str) -> CdpResult<Value> {
    // 极简 HTTP/1.1 GET，只用于本机 DevTools 端点。
    // 注意：新版 Chrome 的 DevTools HTTP 服务不响应 HTTP/1.0 请求（直接断开），
    // 且默认 keep-alive，必须按 Content-Length 读满正文。
    let hostport = url
        .strip_prefix("http://")
        .ok_or("DevTools address is not http")?;
    let (hostport, path) = match hostport.find('/') {
        Some(i) => (&hostport[..i], &hostport[i..]),
        None => (hostport, "/"),
    };
    let mut stream = tokio::net::TcpStream::connect(hostport)
        .await
        .map_err(|e| format!("failed to connect to DevTools: {e}"))?;
    let req = format!(
        "GET {path} HTTP/1.1\r\nHost: {hostport}\r\nAccept: */*\r\nConnection: close\r\n\r\n"
    );
    stream
        .write_all(req.as_bytes())
        .await
        .map_err(|e| e.to_string())?;
    let read_all = async {
        let mut buf = Vec::new();
        let mut chunk = [0u8; 4096];
        loop {
            let n = stream.read(&mut chunk).await.map_err(|e| e.to_string())?;
            if n == 0 {
                break;
            }
            buf.extend_from_slice(&chunk[..n]);
            // 已读到完整正文就停（响应头里有 Content-Length）
            let text = String::from_utf8_lossy(&buf);
            if let Some(hend) = text.find("\r\n\r\n") {
                let headers = text[..hend].to_lowercase();
                if let Some(cl) = headers
                    .split("content-length:")
                    .nth(1)
                    .and_then(|s| s.split("\r\n").next())
                    .and_then(|s| s.trim().parse::<usize>().ok())
                {
                    if text.len() >= hend + 4 + cl {
                        break;
                    }
                }
            }
        }
        Ok::<Vec<u8>, String>(buf)
    };
    let buf = tokio::time::timeout(std::time::Duration::from_secs(5), read_all)
        .await
        .map_err(|_| "DevTools read timed out".to_string())??;
    let text = String::from_utf8_lossy(&buf);
    let body = text
        .split("\r\n\r\n")
        .nth(1)
        .ok_or("DevTools response has no body")?;
    serde_json::from_str(body).map_err(|e| format!("failed to parse DevTools JSON: {e}"))
}

/// 未给 --profile-dir 时的一次性 profile 目录（本进程专属；退出时由
/// main 清理）
pub fn temp_profile_dir() -> std::path::PathBuf {
    std::env::temp_dir().join(format!("rig-edge-{}", std::process::id()))
}

impl Browser {
    pub async fn launch(
        chromium: Option<&str>,
        width: u32,
        height: u32,
        start_url: Option<&str>,
        profile_dir: Option<&str>,
        cache_dir: Option<&str>,
        pulse_server: Option<&str>,
    ) -> CdpResult<Arc<Browser>> {
        let exe = find_chromium(chromium)?;
        let port = {
            let l = std::net::TcpListener::bind("127.0.0.1:0").map_err(|e| e.to_string())?;
            let p = l.local_addr().map_err(|e| e.to_string())?.port();
            drop(l);
            p
        };
        // 持久化双目录（LIU 2026-11-20 定）：--profile-dir 给定则
        // 用它作 Chromium 用户数据目录——Cookie、LocalStorage、
        // IndexedDB、会话存储、Service Worker 与 Cache Storage、
        // 保存的密码、扩展、应用、站点权限全在其中，边缘重启与
        // 整目录迁移后原样续用；不给则回落一次性临时目录（旧
        // 行为）。--cache-dir 单独承接可随时重取的磁盘缓存
        // （HTTP/媒体），不进 profile、迁移时可弃。
        let profile = match profile_dir {
            Some(d) => {
                let p = std::path::PathBuf::from(d);
                let _ = std::fs::create_dir_all(&p);
                p
            }
            None => temp_profile_dir(),
        };
        let cache_arg = cache_dir.map(|d| {
            let _ = std::fs::create_dir_all(d);
            format!("--disk-cache-dir={d}")
        });
        let mut argv: Vec<String> = vec!["--headless=new".to_string()];
        // 沙箱默认保留（GPT review R-05）：仅 root 运行（Chromium 在 root 下
        // 无法启用沙箱）或显式 RIG_CHROMIUM_NO_SANDBOX=1 时才禁用。
        if running_as_root() || std::env::var("RIG_CHROMIUM_NO_SANDBOX").is_ok() {
            argv.push("--no-sandbox".to_string());
            argv.push("--disable-setuid-sandbox".to_string());
        }
        // 人格语言：驱动 navigator.language(s) 与 Accept-Language。
        // RIG_EDGE_LANG 覆盖，默认 ja-JP（同 persona_tz 的口径）。
        let lang = std::env::var("RIG_EDGE_LANG")
            .ok()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "ja-JP".to_string());
        argv.push(format!("--lang={lang}"));
        // 密码存储固定 basic：OSCrypt 密钥随 profile 的 Local State
        // 走（固定口令混淆），整目录搬到新机器后 Cookie 与保存的
        // 密码仍可解密；若用宿主 keyring，密钥留在旧机器、迁移即
        // 全灭。个人受信环境下以可迁移性优先（LIU 2026-11-20）。
        argv.push("--password-store=basic".to_string());
        if let Some(c) = &cache_arg {
            argv.push(c.clone());
        }
        let mut command = tokio::process::Command::new(&exe);
        if let Some(server) = pulse_server {
            command
                .env("PULSE_SERVER", server)
                .env("PULSE_SINK", crate::pulse::SINK_NAME);
        }
        let child = command
            .args(&argv)
            .args([
                // 不再 --disable-gpu：它会让 WebGL 整体不可用——真
                // Chrome 恒有 WebGL（最差也是软件渲染），无 WebGL
                // 既是硬机器人信号也直接弄坏地图/播放器/验证码站。
                // 无 GPU 主机 Chromium 自动回落 SwiftShader；新版
                // Chrome 的软件 WebGL 回落需要显式放行。
                "--enable-unsafe-swiftshader",
                "--disable-dev-shm-usage",
                "--no-first-run",
                "--no-default-browser-check",
                // 自拉的 PulseAudio 可用时，Chromium 声音进入独立的虚拟声卡。
                "--autoplay-policy=no-user-gesture-required",
                &format!("--remote-debugging-port={port}"),
                &format!("--user-data-dir={}", profile.display()),
                &format!("--window-size={width},{height}"),
                // 起始 URL 直接作初始标签页（与真 Chrome 一致：
                // 历史只有一条，后退在起始页上禁用）；先开
                // about:blank 再 navigate 会在历史里留一条空白
                // 条目，后退按钮错误点亮且点下去是白页。
                start_url.unwrap_or("about:blank"),
            ])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .map_err(|e| format!("failed to launch Chromium: {e}"))?;

        // 等 DevTools 端点就绪
        let version_url = format!("http://127.0.0.1:{port}/json/version");
        let mut ws_url = String::new();
        for _ in 0..60 {
            if let Ok(v) = http_get_json(&version_url).await {
                if let Some(u) = v.get("webSocketDebuggerUrl").and_then(Value::as_str) {
                    ws_url = u.to_string();
                    break;
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(250)).await;
        }
        if ws_url.is_empty() {
            return Err("Chromium DevTools endpoint is not ready".into());
        }
        let cdp = Cdp::connect(&ws_url).await?;
        let browser = Self::finish_connect(cdp, Some(child)).await?;
        // 视口必须用设备指标覆盖钉死：新版 headless 的 --window-size
        // 只定窗口、视口被虚拟屏（默认 800x600）裁剪（实测请求 900x700
        // 只得到 900x500），--width/--height 的承诺要靠这里兑现。
        // 挂靠模式不走这里（用浏览器自己的尺寸）。
        let main = browser.main().await?;
        browser.set_viewport(&main, width, height).await?;
        Ok(browser)
    }

    /// 挂靠一个已在运行的 Chromium（它自己开了 --remote-debugging-port）：
    /// 边缘不拉浏览器、退出时也不杀浏览器（child 为 None），client 与
    /// 浏览器由此分开部署（LIU 2026-11-19）。窗口尺寸用浏览器自己的。
    pub async fn attach(cdp_base: &str) -> CdpResult<Arc<Browser>> {
        let base = cdp_base.trim_end_matches('/');
        let version_url = format!("{base}/json/version");
        let mut ws_url = String::new();
        for _ in 0..40 {
            if let Ok(v) = http_get_json(&version_url).await {
                if let Some(u) = v.get("webSocketDebuggerUrl").and_then(Value::as_str) {
                    ws_url = u.to_string();
                    break;
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(250)).await;
        }
        if ws_url.is_empty() {
            return Err(format!(
                "Attach failed: {version_url} did not provide a webSocketDebuggerUrl"
            ));
        }
        let cdp = Cdp::connect(&ws_url).await?;
        Self::finish_connect(cdp, None).await
    }

    async fn finish_connect(
        cdp: Arc<Cdp>,
        child: Option<tokio::process::Child>,
    ) -> CdpResult<Arc<Browser>> {
        let persona_tz = if child.is_some() {
            Some(
                std::env::var("RIG_EDGE_TZ")
                    .ok()
                    .filter(|t| !t.is_empty())
                    .unwrap_or_else(|| "Asia/Tokyo".to_string()),
            )
        } else {
            None
        };
        let browser = Arc::new(Browser {
            cdp,
            child: Mutex::new(child),
            sessions: Mutex::new(HashMap::new()),
            main_session: Mutex::new((None, 0)),
            ua: Mutex::new(None),
            ua_meta: Mutex::new(None),
            accept_lang: Mutex::new(None),
            persona_tz,
        });
        let owned = browser.child.lock().await.is_some();

        // UA 规范化（挂载任何页面会话之前算好）：版本取自浏览器
        // 本体，只把 HeadlessChrome 换成 Chrome；RIG_EDGE_UA 优先。
        // 挂靠模式若挂的是真实浏览器（UA 无 Headless 字样），整段
        // 跳过——不碰用户自己浏览器的身份（含其真实 UA-CH）。
        {
            let ver = browser
                .cdp
                .call(None, "Browser.getVersion", json!({}))
                .await
                .ok();
            let raw_ua = ver
                .as_ref()
                .and_then(|v| v.get("userAgent"))
                .and_then(Value::as_str)
                .map(str::to_string);
            let product = ver
                .as_ref()
                .and_then(|v| v.get("product"))
                .and_then(Value::as_str)
                .map(str::to_string);
            let custom = std::env::var("RIG_EDGE_UA").ok().filter(|u| !u.is_empty());
            let headless = raw_ua
                .as_deref()
                .map(|u| u.contains("Headless"))
                .unwrap_or(false)
                || product
                    .as_deref()
                    .map(|p| p.contains("Headless"))
                    .unwrap_or(false);
            if owned || headless || custom.is_some() {
                let ua = custom
                    .clone()
                    .or_else(|| raw_ua.map(|s| s.replace("HeadlessChrome", "Chrome")));
                // 完整版本号：product 的版本段优先——UA 串里的
                // Chrome/x.y 是约简版本（159.0.0.0），与浏览器按
                // 真实版本回填的 uaFullVersion 对不上（实测教训）
                let full_ver = product
                    .as_deref()
                    .and_then(|p| p.split('/').nth(1).map(str::to_string))
                    .or_else(|| {
                        ua.as_deref().and_then(|u| {
                            u.split("Chrome/")
                                .nth(1)
                                .and_then(|t| t.split([' ', ')']).next())
                                .map(str::to_string)
                        })
                    });
                if let (Some(ua), Some(fv)) = (ua, full_ver) {
                    let major = fv.split('.').next().unwrap_or("").to_string();
                    let meta = json!({
                        "brands": [
                            {"brand": "Chromium", "version": major},
                            {"brand": "Not-A.Brand", "version": "99"}
                        ],
                        "fullVersionList": [
                            {"brand": "Chromium", "version": fv},
                            {"brand": "Not-A.Brand", "version": "99.0.0.0"}
                        ],
                        "platform": "Linux",
                        "platformVersion": "",
                        "architecture": "x86",
                        "bitness": "64",
                        "model": "",
                        "mobile": false,
                        "wow64": false,
                        "formFactors": ["Desktop"]
                    });
                    *browser.ua.lock().await = Some(ua);
                    *browser.ua_meta.lock().await = Some(meta);
                    let lang = std::env::var("RIG_EDGE_LANG")
                        .ok()
                        .filter(|s| !s.is_empty())
                        .unwrap_or_else(|| "ja-JP".to_string());
                    // 注意不带 q 值：CDP 的 acceptLanguage 会被
                    // 原样拆进 navigator.languages，带 ;q= 的条目
                    // 在真浏览器里永不出现（实测教训）；请求头少
                    // q 值只是小瑕疵，JS 可见面优先
                    let base = lang.split('-').next().unwrap_or("").to_string();
                    let al = if lang.starts_with("en") {
                        lang.clone()
                    } else if base != lang {
                        format!("{lang},{base},en")
                    } else {
                        format!("{lang},en")
                    };
                    *browser.accept_lang.lock().await = Some(al);
                }
            }
        }

        // 会话登记：跟踪 attached/detached
        {
            let b = browser.clone();
            let mut rx = b.cdp.subscribe();
            tokio::spawn(async move {
                loop {
                    match rx.recv().await {
                        Ok(ev) => match ev.method.as_str() {
                            "Target.attachedToTarget" => {
                                let sid = ev.params.get("sessionId").and_then(Value::as_str);
                                let info = ev.params.get("targetInfo");
                                if let (Some(sid), Some(info)) = (sid, info) {
                                    let t = SessionInfo {
                                        target_id: info
                                            .get("targetId")
                                            .and_then(Value::as_str)
                                            .unwrap_or("")
                                            .to_string(),
                                        target_type: info
                                            .get("type")
                                            .and_then(Value::as_str)
                                            .unwrap_or("")
                                            .to_string(),
                                        parent: ev.session.clone(),
                                    };
                                    let sid = sid.to_string();
                                    if std::env::var("RIG_EDGE_DEBUG").is_ok() {
                                        eprintln!(
                                            "[debug] attached: type={} url={}",
                                            t.target_type,
                                            url_for_log(
                                                info.get("url")
                                                    .and_then(Value::as_str)
                                                    .unwrap_or("")
                                            )
                                        );
                                    }
                                    let is_page = t.target_type == "page";
                                    let is_frame = is_page || t.target_type == "iframe";
                                    b.sessions.lock().await.insert(sid.clone(), t);
                                    if is_page {
                                        let mut main = b.main_session.lock().await;
                                        if main.0.is_none() {
                                            *main = (Some(sid.clone()), main.1 + 1);
                                        }
                                    }
                                    // 跨进程 iframe 的目标挂在页面会话下，不在浏览器级：
                                    // 每个页面/iframe 会话都要再设一次 autoAttach，
                                    // 否则 OOPIF 永远不出现（实测教训）。
                                    if is_frame {
                                        let _ = b
                                            .cdp
                                            .call(
                                                Some(&sid),
                                                "Target.setAutoAttach",
                                                json!({"autoAttach": true, "waitForDebuggerOnStart": false, "flatten": true}),
                                            )
                                            .await;
                                    }
                                    if is_page {
                                        if let Some(ua) = b.ua.lock().await.clone() {
                                            let mut params = json!({"userAgent": ua});
                                            if let Some(meta) = b.ua_meta.lock().await.clone() {
                                                params["userAgentMetadata"] = meta;
                                            }
                                            if let Some(al) = b.accept_lang.lock().await.clone() {
                                                params["acceptLanguage"] = json!(al);
                                            }
                                            let _ = b
                                                .cdp
                                                .call(
                                                    Some(&sid),
                                                    "Emulation.setUserAgentOverride",
                                                    params,
                                                )
                                                .await;
                                        }
                                        if let Some(tz) = &b.persona_tz {
                                            let _ = b
                                                .cdp
                                                .call(
                                                    Some(&sid),
                                                    "Emulation.setTimezoneOverride",
                                                    json!({"timezoneId": tz}),
                                                )
                                                .await;
                                        }
                                    }
                                    // 自动附加的新标签（window.open / target=_blank）会停在
                                    // 「等调试器」状态，不放行就永远停在 about:blank，用户得
                                    // 手动刷新才加载（对 waitForDebuggerOnStart=false 也成立，
                                    // 2026-11-20 裸 Chrome 对照实测）。放行对已运行页无副作用。
                                    if is_page {
                                        let _ = b
                                            .cdp
                                            .call(
                                                Some(&sid),
                                                "Runtime.runIfWaitingForDebugger",
                                                json!({}),
                                            )
                                            .await;
                                    }
                                    let _ = b.cdp.call(Some(&sid), "Page.enable", json!({})).await;
                                    let _ = b.cdp.call(Some(&sid), "DOM.enable", json!({})).await;
                                    // 不启用 Accessibility 域时，新版 Chrome 的
                                    // getFullAXTree 只返回根节点（懒计算）
                                    let _ = b
                                        .cdp
                                        .call(Some(&sid), "Accessibility.enable", json!({}))
                                        .await;
                                }
                            }
                            "Target.detachedFromTarget" => {
                                if let Some(sid) =
                                    ev.params.get("sessionId").and_then(Value::as_str)
                                {
                                    b.sessions.lock().await.remove(sid);
                                    // 主会话消失时清空，避免 main() 长期指向
                                    // 死会话；后续挂靠/视图切换会重新设定
                                    let mut main = b.main_session.lock().await;
                                    if main.0.as_deref() == Some(sid) {
                                        *main = (None, main.1 + 1);
                                    }
                                }
                            }
                            _ => {}
                        },
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    }
                }
            });
        }

        browser
            .cdp
            .call(None, "Target.setDiscoverTargets", json!({"discover": true}))
            .await?;
        browser
            .cdp
            .call(
                None,
                "Target.setAutoAttach",
                json!({"autoAttach": true, "waitForDebuggerOnStart": false, "flatten": true}),
            )
            .await?;

        // 等主页面会话出现
        for _ in 0..60 {
            if browser.main_session.lock().await.0.is_some() {
                return Ok(browser);
            }
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        }
        Err("main page session did not appear".into())
    }

    pub async fn main(&self) -> CdpResult<String> {
        Ok(self.active_page().await?.session)
    }

    pub async fn active_page(&self) -> CdpResult<ActivePage> {
        let main = self.main_session.lock().await;
        let session = main.0.clone().ok_or("no main page session".to_string())?;
        Ok(ActivePage {
            session,
            generation: main.1,
        })
    }

    /// 主文档的 loaderId（每次导航换新）：采集前后的页面代次
    /// 探针（R-14）——两次不同即期间发生过导航
    pub async fn loader_id(&self, session: &str) -> CdpResult<String> {
        let v = self
            .cdp
            .call(Some(session), "Page.getFrameTree", json!({}))
            .await?;
        v.pointer("/frameTree/frame/loaderId")
            .and_then(Value::as_str)
            .map(str::to_string)
            .ok_or_else(|| "getFrameTree returned no loaderId".to_string())
    }

    /// 把主会话切到指定页面会话。人通道切换活动标签时调用：
    /// AI 通道的观察/动作对象始终跟随人正在看的标签（§7/B4）。
    pub async fn set_main(&self, session: &str) {
        let mut main = self.main_session.lock().await;
        if main.0.as_deref() != Some(session) {
            *main = (Some(session.to_string()), main.1 + 1);
        }
    }

    pub async fn shutdown(&self) {
        if let Some(mut child) = self.child.lock().await.take() {
            // 先请 Chromium 自己关：Browser.close 会把 Cookie 与
            // 各类存储落盘后退出——持久化目录的意义在此兑现。
            // 硬杀只作兜底：SIGKILL 会丢掉最近未提交的写入
            // （2026-11-20 IT 实测：cookie/localStorage 在写入
            // 数秒后被强杀即丢失，IndexedDB 因事务即时提交幸免）。
            let _ = tokio::time::timeout(
                std::time::Duration::from_secs(3),
                self.cdp.call(None, "Browser.close", json!({})),
            )
            .await;
            if tokio::time::timeout(std::time::Duration::from_secs(3), child.wait())
                .await
                .is_err()
            {
                let _ = child.kill().await;
            }
        }
    }

    // ── 页面门面 ─────────────────────────────────────────────

    pub async fn ax_tree(&self, session: &str) -> CdpResult<Vec<Value>> {
        let mut r = self
            .cdp
            .call(Some(session), "Accessibility.getFullAXTree", json!({}))
            .await?;
        match r.get_mut("nodes").map(Value::take) {
            Some(Value::Array(nodes)) => Ok(nodes),
            _ => Ok(Vec::new()),
        }
    }

    /// 单节点 AX（不取亲属）：动作前快速复核用，字段口径与
    /// getFullAXTree 的节点一致，可直接喂 `snapshot::element_core`。
    pub async fn ax_node(&self, session: &str, backend: i64) -> CdpResult<Option<Value>> {
        let r = self
            .cdp
            .call(
                Some(session),
                "Accessibility.getPartialAXTree",
                json!({"backendNodeId": backend, "fetchRelatives": false}),
            )
            .await?;
        Ok(r.get("nodes")
            .and_then(Value::as_array)
            .and_then(|a| {
                a.iter()
                    .find(|n| n.get("backendDOMNodeId").and_then(Value::as_i64) == Some(backend))
            })
            .cloned())
    }

    /// 单节点 DOM 属性（与 `collect_dom` 同口径的 attributes 表）。
    pub async fn dom_attrs(
        &self,
        session: &str,
        backend: i64,
    ) -> CdpResult<HashMap<String, String>> {
        let r = self
            .cdp
            .call(
                Some(session),
                "DOM.describeNode",
                json!({"backendNodeId": backend}),
            )
            .await?;
        let mut attrs = HashMap::new();
        if let Some(arr) = r.pointer("/node/attributes").and_then(Value::as_array) {
            let mut it = arr.iter();
            while let (Some(k), Some(v)) = (it.next(), it.next()) {
                if let (Some(k), Some(v)) = (k.as_str(), v.as_str()) {
                    attrs.insert(k.to_string(), v.to_string());
                }
            }
        }
        Ok(attrs)
    }

    /// 整页 DOM 映射：backendNodeId → DomInfo
    pub async fn dom_map(&self, session: &str) -> CdpResult<DomMap> {
        let r = self
            .cdp
            .call(
                Some(session),
                "DOM.getDocument",
                json!({"depth": -1, "pierce": true}),
            )
            .await?;
        let root = r
            .get("root")
            .filter(|node| node.is_object())
            .ok_or("DOM.getDocument returned no root node")?;
        let mut map = HashMap::new();
        collect_dom(root, &mut map);
        Ok(map)
    }

    /// 截图打码使用完整边框。四个角都必须是有效的视口坐标。
    pub async fn border_bounds(&self, session: &str, backend: i64) -> CdpResult<[f32; 4]> {
        let r = self
            .cdp
            .call(
                Some(session),
                "DOM.getBoxModel",
                json!({"backendNodeId": backend}),
            )
            .await?;
        let points = r
            .pointer("/model/border")
            .and_then(Value::as_array)
            .filter(|p| p.len() == 8)
            .ok_or("DOM.getBoxModel returned no complete border")?;
        let mut bounds = [
            f32::INFINITY,
            f32::INFINITY,
            f32::NEG_INFINITY,
            f32::NEG_INFINITY,
        ];
        for (i, point) in points.iter().enumerate() {
            let v = point
                .as_f64()
                .map(|v| v as f32)
                .filter(|v| v.is_finite())
                .ok_or("DOM.getBoxModel returned no valid border coordinates")?;
            let axis = i % 2;
            bounds[axis] = bounds[axis].min(v);
            bounds[axis + 2] = bounds[axis + 2].max(v);
        }
        if bounds[0] >= bounds[2] || bounds[1] >= bounds[3] {
            return Err("DOM.getBoxModel border is degenerate".into());
        }
        Ok(bounds)
    }

    pub async fn root_frame_id(&self, session: &str) -> CdpResult<String> {
        let r = self
            .cdp
            .call(Some(session), "Page.getFrameTree", json!({}))
            .await?;
        r.pointer("/frameTree/frame/id")
            .and_then(Value::as_str)
            .map(str::to_string)
            .ok_or_else(|| "getFrameTree returned no frame id".to_string())
    }

    /// 取页面已加载的图片资源字节（base64）：跨源图片在页内
    /// canvas 会污染读不到像素，而 Page.getResourceContent 直接
    /// 取浏览器缓存里的原始字节，不受同源限制。按 url 在整棵
    /// frame 资源树里找 frameId。
    pub async fn image_resource_b64(&self, session: &str, url: &str) -> CdpResult<String> {
        fn find(node: &Value, url: &str) -> Option<String> {
            let fid = node.pointer("/frame/id").and_then(Value::as_str)?;
            let hit = node
                .get("resources")
                .and_then(Value::as_array)
                .is_some_and(|rs| {
                    rs.iter()
                        .any(|r| r.get("url").and_then(Value::as_str) == Some(url))
                });
            if hit {
                return Some(fid.to_string());
            }
            node.get("childFrames")
                .and_then(Value::as_array)?
                .iter()
                .find_map(|c| find(c, url))
        }
        let tree = self
            .cdp
            .call(Some(session), "Page.getResourceTree", json!({}))
            .await?;
        let fid = tree
            .get("frameTree")
            .and_then(|t| find(t, url))
            .ok_or("the image is not in the resource tree")?;
        let r = self
            .cdp
            .call(
                Some(session),
                "Page.getResourceContent",
                json!({"frameId": fid, "url": url}),
            )
            .await?;
        let content = r
            .get("content")
            .and_then(Value::as_str)
            .ok_or("getResourceContent returned no content")?;
        if r.get("base64Encoded").and_then(Value::as_bool) == Some(true) {
            Ok(content.to_string())
        } else {
            Err("the image resource is not binary".to_string())
        }
    }

    /// 整页几何批量采集（R-16 定向批量化）：一次
    /// DOMSnapshot.captureSnapshot 拿到全页布局盒，返回
    /// backendNodeId → 视口坐标 [x1,y1,x2,y2]。DOMSnapshot 的
    /// bounds 是文档坐标，这里减去本会话的滚动偏移
    /// （cssVisualViewport.pageX/pageY）换算到与 quads 相同的
    /// 视口口径（dbg33 探针实测：覆盖全部有布局盒的节点、
    /// 坐标与逐节点 quads 一致）。失败返回空表，调用方回落
    /// 逐节点 quads，不改变语义
    pub async fn geometry_map(
        &self,
        session: &str,
        origin: Option<[f32; 2]>,
    ) -> HashMap<i64, [f32; 4]> {
        let mut out: std::collections::HashMap<i64, [f32; 4]> = std::collections::HashMap::new();
        let Ok(r) = self
            .cdp
            .call(
                Some(session),
                "DOMSnapshot.captureSnapshot",
                // 只使用 layout.bounds，避免额外传输 offset、client 和 scroll 矩形。
                json!({"computedStyles": [], "includePaintOrder": false}),
            )
            .await
        else {
            return out;
        };
        let [px, py] = match origin {
            Some(origin) => origin,
            None => {
                let layout = self
                    .cdp
                    .call(Some(session), "Page.getLayoutMetrics", json!({}))
                    .await;
                let scroll = |key| {
                    layout
                        .as_ref()
                        .ok()
                        .and_then(|m| m.get("cssVisualViewport"))
                        .and_then(|v| v.get(key))
                        .and_then(Value::as_f64)
                        .unwrap_or(0.0) as f32
                };
                [scroll("pageX"), scroll("pageY")]
            }
        };
        let Some(docs) = r.get("documents").and_then(Value::as_array) else {
            return out;
        };
        for doc in docs {
            let Some(backend) = doc
                .pointer("/nodes/backendNodeId")
                .and_then(Value::as_array)
            else {
                continue;
            };
            let Some(layout) = doc.get("layout") else {
                continue;
            };
            let (Some(index), Some(bounds)) = (
                layout.get("nodeIndex").and_then(Value::as_array),
                layout.get("bounds").and_then(Value::as_array),
            ) else {
                continue;
            };
            for (li, ni) in index.iter().enumerate() {
                let (Some(ni), Some(b)) = (ni.as_u64(), bounds.get(li).and_then(Value::as_array))
                else {
                    continue;
                };
                let Some(bid) = backend.get(ni as usize).and_then(Value::as_i64) else {
                    continue;
                };
                let nums: Vec<f32> = b
                    .iter()
                    .filter_map(|v| v.as_f64().map(|f| f as f32))
                    .collect();
                if nums.len() < 4 {
                    continue;
                }
                // 同节点多条布局记录取第一条（与 quads 取首个
                // quad 的口径一致）
                out.entry(bid).or_insert([
                    nums[0] - px,
                    nums[1] - py,
                    nums[0] + nums[2] - px,
                    nums[1] + nums[3] - py,
                ]);
            }
        }
        out
    }

    /// 元素在该会话自身视口坐标系中的包围盒 [x1,y1,x2,y2]
    pub async fn quads(&self, session: &str, backend: i64) -> CdpResult<Option<[f32; 4]>> {
        // R-16 量测点：逐节点几何查询的次数与耗时都记账
        let started = std::time::Instant::now();
        let out = self.quads_inner(session, backend).await;
        crate::metrics::global().quads.record(started.elapsed());
        out
    }

    async fn quads_inner(&self, session: &str, backend: i64) -> CdpResult<Option<[f32; 4]>> {
        let r = self
            .cdp
            .call(
                Some(session),
                "DOM.getContentQuads",
                json!({"backendNodeId": backend}),
            )
            .await?;
        let Some(first) = r
            .get("quads")
            .and_then(Value::as_array)
            .and_then(|a| a.first())
            .and_then(Value::as_array)
        else {
            return Ok(None);
        };
        let nums: Vec<f32> = first
            .iter()
            .filter_map(|v| v.as_f64().map(|f| f as f32))
            .collect();
        if nums.len() < 8 {
            return Ok(None);
        }
        let (mut x1, mut y1, mut x2, mut y2) = (f32::MAX, f32::MAX, f32::MIN, f32::MIN);
        for i in (0..8).step_by(2) {
            x1 = x1.min(nums[i]);
            x2 = x2.max(nums[i]);
            y1 = y1.min(nums[i + 1]);
            y2 = y2.max(nums[i + 1]);
        }
        Ok(Some([x1, y1, x2, y2]))
    }

    /// 快照使用的坐标空间和视觉视口。
    pub async fn view_state(&self, session: &str) -> CdpResult<ViewState> {
        let r = self
            .cdp
            .call(Some(session), "Page.getLayoutMetrics", json!({}))
            .await?;
        let vv = r
            .get("cssVisualViewport")
            .ok_or("getLayoutMetrics returned no cssVisualViewport")?;
        let f = |key: &str| {
            vv.get(key)
                .and_then(Value::as_f64)
                .map(|v| v as f32)
                .filter(|v| v.is_finite())
                .ok_or_else(|| format!("cssVisualViewport has no valid {key}"))
        };
        let d = self
            .cdp
            .call(
                Some(session),
                "Runtime.evaluate",
                json!({"expression": "devicePixelRatio", "returnByValue": true}),
            )
            .await?;
        let space = rig_proto::observation::Space {
            width: f("clientWidth")?,
            height: f("clientHeight")?,
            scale: d
                .pointer("/result/value")
                .and_then(Value::as_f64)
                .ok_or("cannot obtain devicePixelRatio")? as f32,
        };
        let visual_scale = f("scale")?;
        if !space.is_valid() || visual_scale <= 0.0 {
            return Err("viewport coordinate space is invalid".into());
        }
        Ok(ViewState {
            space,
            page_origin: [f("pageX")?, f("pageY")?],
            visual_offset: [f("offsetX")?, f("offsetY")?],
            visual_scale,
        })
    }

    /// (CSS 视口宽, 高, devicePixelRatio)
    pub async fn layout(&self, session: &str) -> CdpResult<(f32, f32, f32)> {
        let space = self.view_state(session).await?.space;
        Ok((space.width, space.height, space.scale))
    }

    /// 执行 JS 并返回结果值（awaitPromise）。用于剪贴板读回等页面侧异步 API。
    pub async fn eval_json(&self, session: &str, expr: &str) -> CdpResult<Value> {
        let r = self
            .cdp
            .call(
                Some(session),
                "Runtime.evaluate",
                json!({"expression": expr, "returnByValue": true, "awaitPromise": true}),
            )
            .await?;
        if let Some(exc) = r.get("exceptionDetails") {
            return Err(format!("page JS exception: {exc}"));
        }
        Ok(r.pointer("/result/value").cloned().unwrap_or(Value::Null))
    }

    /// 授予剪贴板权限（异步剪贴板 API 需要），浏览器级调用。
    pub async fn grant_clipboard(&self, origin: &str) {
        let _ = self
            .cdp
            .call(
                None,
                "Browser.grantPermissions",
                json!({"origin": origin, "permissions": ["clipboardReadWrite", "clipboardSanitizedWrite"]}),
            )
            .await;
    }

    /// 当前 CSS 视口坐标实际命中的 DOM 节点。
    async fn hit_backend_node(&self, session: &str, x: f32, y: f32) -> CdpResult<i64> {
        let hit = self
            .cdp
            .call(
                Some(session),
                "DOM.getNodeForLocation",
                json!({"x": x.round() as i32, "y": y.round() as i32, "ignorePointerEventsNone": false}),
            )
            .await?;
        hit.get("backendNodeId")
            .and_then(Value::as_i64)
            .ok_or_else(|| "hit test returned no node".to_string())
    }

    pub async fn element_receives_pointer(
        &self,
        session: &str,
        backend: i64,
        x: f32,
        y: f32,
    ) -> CdpResult<bool> {
        let hit = self.hit_backend_node(session, x, y).await?;
        if hit == backend {
            return Ok(true);
        }
        // 按钮内部的文本或图标可以接收指针，但遮住按钮的兄弟节点不行。
        let tree = self
            .cdp
            .call(
                Some(session),
                "DOM.describeNode",
                json!({
                    "backendNodeId": backend, "depth": -1, "pierce": true,
                }),
            )
            .await?;
        Ok(tree
            .get("node")
            .is_some_and(|node| dom_contains_backend(node, hit)))
    }

    /// 子会话可直接接收 CDP 输入，仍须确认同一点在每层父会话命中 iframe 宿主。
    pub async fn frame_receives_pointer(
        &self,
        main: &str,
        session: &str,
        mut x: f32,
        mut y: f32,
    ) -> CdpResult<bool> {
        let mut session = session.to_owned();
        while session != main {
            let parent = self
                .sessions
                .lock()
                .await
                .get(&session)
                .and_then(|info| info.parent.clone())
                .ok_or("subframe session has no parent session")?;
            let view = self.view_state(&session).await?.space;
            if !view.contains(x, y) {
                return Ok(false);
            }
            let frame_id = self.root_frame_id(&session).await?;
            let owner = self
                .cdp
                .call(
                    Some(&parent),
                    "DOM.getFrameOwner",
                    json!({"frameId": frame_id}),
                )
                .await?;
            let backend = owner
                .get("backendNodeId")
                .and_then(Value::as_i64)
                .ok_or("cannot locate the subframe host")?;
            let model = self
                .cdp
                .call(
                    Some(&parent),
                    "DOM.getBoxModel",
                    json!({"backendNodeId": backend}),
                )
                .await?;
            let content = model
                .pointer("/model/content")
                .and_then(Value::as_array)
                .filter(|quad| quad.len() == 8)
                .ok_or("subframe host has no valid content border")?;
            let mut quad = [0.0; 8];
            for (point, value) in quad.iter_mut().zip(content) {
                *point = value
                    .as_f64()
                    .map(|value| value as f32)
                    .filter(|value| value.is_finite())
                    .ok_or("subframe host coordinates are invalid")?;
            }
            let (u, v) = (x / view.width, y / view.height);
            let project = |axis: usize| {
                let top = quad[axis] + u * (quad[axis + 2] - quad[axis]);
                let bottom = quad[axis + 6] + u * (quad[axis + 4] - quad[axis + 6]);
                top + v * (bottom - top)
            };
            (x, y) = (project(0), project(1));
            if self.hit_backend_node(&parent, x, y).await? != backend {
                return Ok(false);
            }
            session = parent;
        }
        Ok(true)
    }

    pub async fn scroll_into_view(&self, session: &str, backend: i64) -> CdpResult<()> {
        self.cdp
            .call(
                Some(session),
                "DOM.scrollIntoViewIfNeeded",
                json!({"backendNodeId": backend}),
            )
            .await?;
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn mouse(
        &self,
        session: &str,
        kind: &str,
        x: f32,
        y: f32,
        button: Option<&str>,
        click_count: Option<u32>,
        delta: Option<(f32, f32)>,
        buttons: Option<u32>,
    ) -> CdpResult<()> {
        self.mouse_with_modifiers(session, kind, x, y, button, click_count, delta, buttons, 0)
            .await
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn mouse_with_modifiers(
        &self,
        session: &str,
        kind: &str,
        x: f32,
        y: f32,
        button: Option<&str>,
        click_count: Option<u32>,
        delta: Option<(f32, f32)>,
        buttons: Option<u32>,
        modifiers: u32,
    ) -> CdpResult<()> {
        let mut p = json!({"type": kind, "x": x, "y": y, "modifiers": modifiers});
        if let Some(b) = button {
            p["button"] = json!(b);
        }
        // buttons 位掩码：拖动中的 mouseMoved 必须带它，页面才认拖拽
        // （选区/拖放）；pressed/released 也据此标明当前按下集合。
        if let Some(bs) = buttons {
            p["buttons"] = json!(bs);
        }
        if let Some(c) = click_count {
            p["clickCount"] = json!(c);
        }
        if let Some((dx, dy)) = delta {
            p["deltaX"] = json!(dx);
            p["deltaY"] = json!(dy);
        }
        self.cdp
            .call(Some(session), "Input.dispatchMouseEvent", p)
            .await?;
        Ok(())
    }

    pub async fn key(
        &self,
        session: &str,
        kind: &str,
        key: &str,
        code: &str,
        text: Option<&str>,
        modifiers: u32,
    ) -> CdpResult<()> {
        let kind = if kind == "keyDown" {
            "rawKeyDown"
        } else {
            kind
        };
        self.dispatch_key(
            session,
            kind,
            key,
            code,
            text,
            modifiers,
            Self::key_location(code),
            false,
        )
        .await
    }

    // CDP keyDown 一并提交可打印字符。组合键及 AI 通道的独立键／字符事件使用 rawKeyDown。
    #[allow(clippy::too_many_arguments)]
    pub async fn dispatch_key(
        &self,
        session: &str,
        kind: &str,
        key: &str,
        code: &str,
        text: Option<&str>,
        modifiers: u32,
        location: u32,
        repeat: bool,
    ) -> CdpResult<()> {
        let mut p = json!({
            "type": kind, "key": key, "code": code, "modifiers": modifiers,
            "autoRepeat": repeat, "isKeypad": location == 3,
        });
        if matches!(location, 1 | 2) {
            p["location"] = json!(location);
        }
        let vk_code = if code.starts_with("Numpad")
            && matches!(
                key,
                "Insert"
                    | "Delete"
                    | "Home"
                    | "End"
                    | "PageUp"
                    | "PageDown"
                    | "ArrowLeft"
                    | "ArrowRight"
                    | "ArrowUp"
                    | "ArrowDown"
            ) {
            key
        } else {
            code
        };
        if let Some(vk) = Self::virtual_key_code(vk_code) {
            p["windowsVirtualKeyCode"] = json!(vk);
            p["nativeVirtualKeyCode"] = json!(vk);
        }
        if let Some(t) = text {
            p["text"] = json!(t);
            if modifiers & 7 == 0 {
                p["unmodifiedText"] = json!(t);
            }
        }
        self.cdp
            .call(Some(session), "Input.dispatchKeyEvent", p)
            .await?;
        Ok(())
    }

    pub fn key_location(code: &str) -> u32 {
        match code {
            "ShiftLeft" | "ControlLeft" | "MetaLeft" | "AltLeft" => 1,
            "ShiftRight" | "ControlRight" | "MetaRight" | "AltRight" => 2,
            code if code.starts_with("Numpad") => 3,
            _ => 0,
        }
    }

    /// 由 code 推导 Windows 虚拟键码（字母/数字/常用键），推导不出返回 None。
    fn virtual_key_code(code: &str) -> Option<u32> {
        if let Some(rest) = code.strip_prefix("Key") {
            let mut cs = rest.chars();
            if let (Some(c), None) = (cs.next(), cs.next()) {
                if c.is_ascii_uppercase() {
                    return Some(c as u32);
                }
            }
        }
        if let Some(digit) = code.strip_prefix("Digit") {
            if let [n @ b'0'..=b'9'] = digit.as_bytes() {
                return Some(u32::from(*n));
            }
        }
        if let Some(digit) = code.strip_prefix("Numpad") {
            if let [n @ b'0'..=b'9'] = digit.as_bytes() {
                return Some(96 + u32::from(n - b'0'));
            }
        }
        if let Some(number) = code
            .strip_prefix('F')
            .and_then(|key| key.parse::<u32>().ok())
        {
            if (1..=24).contains(&number) {
                return Some(111 + number);
            }
        }
        Some(match code {
            "Enter" | "NumpadEnter" => 13,
            "Tab" => 9,
            "Escape" => 27,
            "Backspace" => 8,
            "Delete" => 46,
            "Space" => 32,
            "ArrowLeft" => 37,
            "ArrowUp" => 38,
            "ArrowRight" => 39,
            "ArrowDown" => 40,
            "Home" => 36,
            "End" => 35,
            "PageUp" => 33,
            "PageDown" => 34,
            "Insert" => 45,
            "ShiftLeft" | "ShiftRight" => 16,
            "ControlLeft" | "ControlRight" => 17,
            "AltLeft" | "AltRight" => 18,
            "CapsLock" => 20,
            "Pause" => 19,
            "PrintScreen" => 44,
            "MetaLeft" => 91,
            "MetaRight" => 92,
            "ContextMenu" => 93,
            "NumLock" => 144,
            "ScrollLock" => 145,
            "NumpadMultiply" => 106,
            "NumpadAdd" => 107,
            "NumpadComma" => 108,
            "NumpadSubtract" => 109,
            "NumpadDecimal" => 110,
            "NumpadDivide" => 111,
            "Semicolon" => 186,
            "Equal" | "NumpadEqual" => 187,
            "Comma" => 188,
            "Minus" => 189,
            "Period" => 190,
            "Slash" => 191,
            "Backquote" => 192,
            "BracketLeft" => 219,
            "Backslash" => 220,
            "BracketRight" => 221,
            "Quote" => 222,
            "IntlBackslash" => 226,
            _ => return None,
        })
    }

    pub async fn insert_text(&self, session: &str, text: &str) -> CdpResult<()> {
        self.cdp
            .call(Some(session), "Input.insertText", json!({"text": text}))
            .await?;
        Ok(())
    }

    pub async fn screenshot_png(&self, session: &str, view: ViewState) -> CdpResult<Vec<u8>> {
        let r = self
            .cdp
            .call(
                Some(session),
                "Page.captureScreenshot",
                json!({
                    "format": "png", "fromSurface": true,
                    "clip": {
                        "x": view.page_origin[0], "y": view.page_origin[1],
                        "width": view.space.width, "height": view.space.height,
                        "scale": view.visual_scale,
                    }
                }),
            )
            .await?;
        let b64 = r
            .get("data")
            .and_then(Value::as_str)
            .ok_or("captureScreenshot returned no data")?;
        use base64::Engine;
        base64::engine::general_purpose::STANDARD
            .decode(b64)
            .map_err(|e| format!("screenshot base64 decode failed: {e}"))
    }

    /// 区域截图（DOM 区域子流用）：对页面表面按视口相对 CSS px
    /// 的 rect 截图，返回 base64 JPEG。CDP 在浏览器层合成，跨源
    /// iframe 内容同样入图（页面脚本受同源策略限制、CDP 不受）。
    pub async fn capture_region(
        &self,
        session: &str,
        x: i64,
        y: i64,
        w: i64,
        h: i64,
    ) -> Option<String> {
        let r = self
            .cdp
            .call(
                Some(session),
                "Page.captureScreenshot",
                json!({"format": "jpeg", "quality": 65, "fromSurface": true,
                       "clip": {"x": x, "y": y, "width": w, "height": h, "scale": 1}}),
            )
            .await
            .ok()?;
        r.get("data").and_then(Value::as_str).map(str::to_string)
    }

    // ── 标签管理（人通道 chrome UI 用，见 docs/EDGE_BROWSER.md §9.1） ──

    /// 某会话对应的 target id
    pub async fn target_id_of(&self, session: &str) -> Option<String> {
        self.sessions
            .lock()
            .await
            .get(session)
            .map(|i| i.target_id.clone())
    }

    /// target 对应的会话 id（页面 target 均经 auto-attach 自动挂上）
    pub async fn session_for_target(&self, target_id: &str) -> Option<String> {
        self.sessions
            .lock()
            .await
            .iter()
            .find(|(_, i)| i.target_id == target_id && i.target_type == "page")
            .map(|(sid, _)| sid.clone())
    }

    /// 全部页面 target：(target_id, title, url)，按 CDP 返回顺序
    pub async fn page_targets(&self) -> CdpResult<Vec<(String, String, String)>> {
        let v = self.cdp.call(None, "Target.getTargets", json!({})).await?;
        let mut out = Vec::new();
        if let Some(arr) = v.get("targetInfos").and_then(Value::as_array) {
            for t in arr {
                if t.get("type").and_then(Value::as_str) == Some("page") {
                    let g = |k: &str| t.get(k).and_then(Value::as_str).unwrap_or("").to_string();
                    out.push((g("targetId"), g("title"), g("url")));
                }
            }
        }
        Ok(out)
    }

    /// 由 `parent` 页面开出的、不在 `known` 里的新标签（window.open /
    /// target=_blank）。靠 targetInfo.openerId 判断，用户自己在别处开的不算。
    pub async fn new_popups(&self, parent: &str, known: &[String]) -> Vec<String> {
        let Ok(v) = self.cdp.call(None, "Target.getTargets", json!({})).await else {
            return Vec::new();
        };
        v.get("targetInfos")
            .and_then(Value::as_array)
            .map(|arr| {
                arr.iter()
                    .filter(|t| t.get("type").and_then(Value::as_str) == Some("page"))
                    .filter(|t| t.get("openerId").and_then(Value::as_str) == Some(parent))
                    .filter_map(|t| t.get("targetId").and_then(Value::as_str))
                    .filter(|id| !known.iter().any(|k| k == id))
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default()
    }

    pub async fn create_target(&self, url: &str) -> CdpResult<String> {
        let v = self
            .cdp
            .call(None, "Target.createTarget", json!({"url": url}))
            .await?;
        v.get("targetId")
            .and_then(Value::as_str)
            .map(str::to_string)
            .ok_or_else(|| "createTarget returned no targetId".to_string())
    }

    pub async fn close_target(&self, target_id: &str) -> CdpResult<()> {
        self.cdp
            .call(None, "Target.closeTarget", json!({"targetId": target_id}))
            .await?;
        Ok(())
    }

    /// 是否由本程序自行拉起的 Chromium（挂靠用户自己的浏览器时为 false）
    pub async fn is_self_launched(&self) -> bool {
        self.child.lock().await.is_some()
    }

    pub async fn activate_target(&self, target_id: &str) -> CdpResult<()> {
        self.cdp
            .call(
                None,
                "Target.activateTarget",
                json!({"targetId": target_id}),
            )
            .await?;
        Ok(())
    }

    /// 导航状态：(可后退, 可前进, 载入中)
    pub async fn nav_state(&self, session: &str) -> (bool, bool, bool) {
        let hist = self
            .cdp
            .call(Some(session), "Page.getNavigationHistory", json!({}))
            .await;
        let (back, fwd) = match &hist {
            Ok(v) => {
                let cur = v.get("currentIndex").and_then(Value::as_i64).unwrap_or(0);
                let n = v
                    .get("entries")
                    .and_then(Value::as_array)
                    .map(|a| a.len() as i64)
                    .unwrap_or(0);
                (cur > 0, cur + 1 < n)
            }
            Err(_) => (false, false),
        };
        let loading = self
            .eval_json(session, "document.readyState")
            .await
            .ok()
            .and_then(|v| v.as_str().map(|s| s != "complete"))
            .unwrap_or(false);
        (back, fwd, loading)
    }

    pub async fn history_go(&self, session: &str, delta: i64) -> CdpResult<()> {
        let v = self
            .cdp
            .call(Some(session), "Page.getNavigationHistory", json!({}))
            .await?;
        let cur = v.get("currentIndex").and_then(Value::as_i64).unwrap_or(0);
        let entries = v
            .get("entries")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let idx = cur + delta;
        if idx < 0 || idx as usize >= entries.len() {
            return Ok(());
        }
        let id = entries[idx as usize]
            .get("id")
            .and_then(Value::as_i64)
            .unwrap_or(0);
        self.cdp
            .call(
                Some(session),
                "Page.navigateToHistoryEntry",
                json!({"entryId": id}),
            )
            .await?;
        Ok(())
    }

    pub async fn reload_page(&self, session: &str) -> CdpResult<()> {
        self.cdp
            .call(Some(session), "Page.reload", json!({}))
            .await?;
        Ok(())
    }

    pub async fn stop_loading(&self, session: &str) -> CdpResult<()> {
        self.cdp
            .call(Some(session), "Page.stopLoading", json!({}))
            .await?;
        Ok(())
    }

    /// 打印当前页为 PDF（供观看端「打印」菜单项经 /print.pdf 取回）
    pub async fn print_pdf(&self, session: &str) -> CdpResult<Vec<u8>> {
        let v = self
            .cdp
            .call(
                Some(session),
                "Page.printToPDF",
                json!({"printBackground": true}),
            )
            .await?;
        let data = v
            .get("data")
            .and_then(Value::as_str)
            .ok_or_else(|| "printToPDF returned no data".to_string())?;
        use base64::Engine;
        base64::engine::general_purpose::STANDARD
            .decode(data)
            .map_err(|e| e.to_string())
    }

    /// 视口尺寸覆盖（分辨率自适应/固定档位）；w=h=0 清除覆盖
    pub async fn set_viewport(&self, session: &str, w: u32, h: u32) -> CdpResult<()> {
        if w == 0 || h == 0 {
            self.cdp
                .call(
                    Some(session),
                    "Emulation.clearDeviceMetricsOverride",
                    json!({}),
                )
                .await?;
        } else {
            // screenWidth/Height 一并钉住：新版 headless 的虚拟屏
            // 默认 800x600，窗口 1280x800 时 screen 比窗口还小——
            // 真实桌面不可能出现，是廉价的异常信号。按「浏览器
            // 最大化」的最常见形态让 screen == 视口。
            self.cdp
                .call(
                    Some(session),
                    "Emulation.setDeviceMetricsOverride",
                    json!({"width": w, "height": h, "deviceScaleFactor": 1, "mobile": false, "screenWidth": w, "screenHeight": h}),
                )
                .await?;
        }
        Ok(())
    }

    pub async fn navigate(&self, session: &str, url: &str) -> CdpResult<()> {
        let response = self
            .cdp
            .call(Some(session), "Page.navigate", json!({"url": url}))
            .await?;
        if let Some(error) = response.get("errorText").and_then(Value::as_str) {
            if !error.is_empty() {
                return Err(format!("Chromium navigation failed: {error}"));
            }
        }
        Ok(())
    }
}

fn dom_contains_backend(node: &Value, backend: i64) -> bool {
    node.get("backendNodeId").and_then(Value::as_i64) == Some(backend)
        || ["children", "shadowRoots"].into_iter().any(|key| {
            node.get(key)
                .and_then(Value::as_array)
                .is_some_and(|nodes| {
                    nodes
                        .iter()
                        .any(|child| dom_contains_backend(child, backend))
                })
        })
}

fn collect_dom(node: &Value, map: &mut DomMap) {
    if let Some(backend) = node.get("backendNodeId").and_then(Value::as_i64) {
        let tag = node
            .get("nodeName")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_lowercase();
        let mut attrs = HashMap::new();
        if let Some(arr) = node.get("attributes").and_then(Value::as_array) {
            let mut it = arr.iter();
            while let (Some(k), Some(v)) = (it.next(), it.next()) {
                if let (Some(k), Some(v)) = (k.as_str(), v.as_str()) {
                    attrs.insert(k.to_string(), v.to_string());
                }
            }
        }
        let frame_id = node
            .get("frameId")
            .and_then(Value::as_str)
            .map(str::to_string);
        map.insert(backend, (tag, attrs, frame_id));
    }
    for key in ["children", "shadowRoots"] {
        if let Some(arr) = node.get(key).and_then(Value::as_array) {
            for c in arr {
                collect_dom(c, map);
            }
        }
    }
    if let Some(cd) = node.get("contentDocument") {
        collect_dom(cd, map);
    }
    if let Some(t) = node.get("templateContent") {
        collect_dom(t, map);
    }
}

#[cfg(test)]
mod chromium_lookup_tests {
    use super::{
        chromium_in_dir, find_on_path, is_executable, path_chromium_names, resolve_chromium,
    };
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicUsize, Ordering};

    static NEXT_DIR: AtomicUsize = AtomicUsize::new(0);

    struct TestDir(PathBuf);

    impl TestDir {
        fn new() -> Self {
            let id = NEXT_DIR.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir()
                .join(format!("rig-chromium-lookup-{}-{id}", std::process::id()));
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }

        fn add_executable(&self, name: &str) -> PathBuf {
            let path = self
                .0
                .join(format!("{name}{}", std::env::consts::EXE_SUFFIX));
            std::fs::write(&path, b"test").unwrap();
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
            }
            path
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn explicit_directory_finds_direct_chrome_or_chromium() {
        let dir = TestDir::new();
        let expected = dir.add_executable("chromium");
        assert_eq!(chromium_in_dir(Path::new(&dir.0)), Some(expected));
    }

    #[test]
    fn bundled_directory_precedes_path_and_explicit_fallback() {
        let bundled = TestDir::new();
        let bundled_exe = bundled.add_executable("chrome");
        let path_dir = TestDir::new();
        path_dir.add_executable("chromium");
        let explicit = TestDir::new();
        explicit.add_executable("chromium");

        let result = resolve_chromium(
            Some(explicit.0.to_str().unwrap()),
            Some(bundled.0.as_path()),
            Some(path_dir.0.as_os_str()),
            &[],
            path_chromium_names(),
        );
        assert_eq!(result.unwrap(), bundled_exe);
    }

    #[test]
    fn path_precedes_installed_directories_and_explicit_fallback() {
        let path_dir = TestDir::new();
        let path_exe = path_dir.add_executable("chromium");
        let installed = TestDir::new();
        installed.add_executable("chrome");
        let explicit = TestDir::new();
        explicit.add_executable("chromium");

        let result = resolve_chromium(
            Some(explicit.0.to_str().unwrap()),
            None,
            Some(path_dir.0.as_os_str()),
            std::slice::from_ref(&installed.0),
            path_chromium_names(),
        );
        assert_eq!(result.unwrap(), path_exe);
    }

    #[test]
    fn installed_directories_precede_explicit_fallback() {
        let installed = TestDir::new();
        let installed_exe = installed.add_executable("chrome");
        let explicit = TestDir::new();
        explicit.add_executable("chromium");

        let result = resolve_chromium(
            Some(explicit.0.to_str().unwrap()),
            None,
            None,
            std::slice::from_ref(&installed.0),
            path_chromium_names(),
        );
        assert_eq!(result.unwrap(), installed_exe);
    }

    #[test]
    fn explicit_directory_is_last_fallback_and_absolute_bundled_only() {
        let explicit = TestDir::new();
        let explicit_exe = explicit.add_executable("chromium");
        let relative = Path::new("relative-bundle");
        assert!(!relative.is_absolute());
        let result = resolve_chromium(
            Some(explicit.0.to_str().unwrap()),
            Some(relative),
            None,
            &[],
            path_chromium_names(),
        );
        assert_eq!(result.unwrap(), explicit_exe);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn chromium_directory_supports_platform_layouts() {
        let dir = TestDir::new();
        let app = dir.0.join("Google Chrome for Testing.app/Contents/MacOS");
        std::fs::create_dir_all(&app).unwrap();
        let app_exe = app.join("Google Chrome for Testing");
        std::fs::write(&app_exe, b"test").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&app_exe, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        assert_eq!(chromium_in_dir(Path::new(&dir.0)), Some(app_exe));
    }

    #[test]
    fn explicit_directory_rejects_nested_or_non_executable_candidates() {
        let dir = TestDir::new();
        std::fs::create_dir(dir.0.join("nested")).unwrap();
        std::fs::write(dir.0.join("nested/chrome"), b"test").unwrap();
        let non_executable = dir.0.join("chrome");
        std::fs::write(&non_executable, b"test").unwrap();
        assert_eq!(chromium_in_dir(Path::new(&dir.0)), None);
    }

    #[test]
    fn invalid_explicit_directory_error_names_option() {
        let dir = TestDir::new();
        let error = resolve_chromium(dir.0.to_str(), None, None, &[], &[]).unwrap_err();
        assert!(error.to_string().contains("--chromium-dir"));
    }

    #[test]
    fn path_lookup_finds_only_executable_named_candidates() {
        let dir = TestDir::new();
        let expected = dir.add_executable("google-chrome");
        assert_eq!(
            find_on_path(
                expected.file_name().unwrap().to_str().unwrap(),
                Some(dir.0.as_os_str())
            ),
            Some(expected)
        );
        assert_eq!(find_on_path("chromium", Some(dir.0.as_os_str())), None);
        assert!(!is_executable(&dir.0.join("missing")));
    }
}
