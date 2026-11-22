//! 端到端集成测试：真 Chromium + 真 HTTP/WS 接口。
//! 需 RIG_EDGE_IT=1 才运行（默认测试保持离线、快速）；运行方式：
//!   RIG_EDGE_IT=1 cargo test -p rig-edge-browser --test edge_it -- --nocapture
//!
//! 覆盖：登录全流程（动作+脱敏）、跨源 iframe 坐标合成、canvas 页截圖、
//! 双通道独立开关（关人通道 WS 被拒、关 AI 通道 observe 被拒）。

use std::io::{Read, Write};
use std::net::TcpListener;
use std::os::unix::process::CommandExt;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use base64::Engine;
use futures_util::StreamExt;
use serde_json::{json, Value};

/// 集成测试串行锁：每项都要拉 Chromium（AV 项还跑 x265 编码），
/// 在共享开发机上全并行会互相挤兑造成假失败——串行执行换确定性。
/// 用 tokio 的锁：可跨 await 持有（clippy 不许 std 锁跨 await）。
static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

const LOGIN_HTML: &str = r#"<!DOCTYPE html><html lang="zh"><head><meta charset="utf-8"><title>登录</title></head>
<body>
<label>用户名 <input id="user" type="text"></label>
<label>密码 <input id="pass" type="password"></label>
<button id="login">登录</button>
<p id="welcome"></p>
<script>
document.getElementById('login').addEventListener('click', () => {
  const u = document.getElementById('user').value;
  const p = document.getElementById('pass').value;
  if (u === 'alice' && p === 'hunter2') {
    document.getElementById('welcome').textContent = '欢迎 alice';
  }
});
</script>
</body></html>"#;

const FORM_HTML: &str = r#"<!DOCTYPE html><html lang="zh"><head><meta charset="utf-8"><title>内层表单</title></head>
<body style="margin:0">
<label>姓名 <input id="name" type="text"></label>
<button id="submit" onclick="document.getElementById('click-count').textContent = '点击次数：1'">提交</button>
<p id="click-count">点击次数：0</p>
<input type="password" aria-label="子帧密码" value="SYNTHETIC-IFRAME">
</body></html>"#;

const CANVAS_HTML: &str = r#"<!DOCTYPE html><html lang="zh"><head><meta charset="utf-8"><title>画布</title></head>
<body>
<canvas id="cv" width="300" height="200"></canvas>
<script>
const c = document.getElementById('cv').getContext('2d');
c.fillStyle = '#c00'; c.fillRect(50, 50, 100, 60);
c.fillStyle = '#fff'; c.font = '16px sans-serif'; c.fillText('目标', 70, 85);
</script>
</body></html>"#;

fn iframe_html(inner_origin: &str) -> String {
    format!(
        r#"<!DOCTYPE html><html lang="zh"><head><meta charset="utf-8"><title>外层</title></head>
<body>
<p>外层页面</p>
<iframe id="inner" title="内层表单" width="420" height="160" style="margin:40px" src="{inner_origin}/form.html"></iframe>
</body></html>"#
    )
}

/// 极简静态文件服务（内存中的页面表），在给定的监听器上服务
fn serve(listener: TcpListener, pages: Vec<(String, String)>) -> u16 {
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut s) = stream else { continue };
            let pages = pages.clone();
            std::thread::spawn(move || {
                let mut buf = [0u8; 4096];
                let n = s.read(&mut buf).unwrap_or(0);
                let req = String::from_utf8_lossy(&buf[..n]).to_string();
                let path = req
                    .split_whitespace()
                    .nth(1)
                    .unwrap_or("/")
                    .split('?')
                    .next()
                    .unwrap_or("/")
                    .to_string();
                let (status, body) = match pages.iter().find(|(p, _)| *p == path) {
                    Some((_, html)) => ("200 OK", html.clone()),
                    None => ("404 Not Found", "not found".to_string()),
                };
                let resp = format!(
                    "HTTP/1.1 {status}\r\ncontent-type: text/html; charset=utf-8\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = s.write_all(resp.as_bytes());
            });
        }
    });
    port
}

fn http(port: u16, method: &str, path: &str, body: Option<&str>) -> (u16, Vec<u8>) {
    let mut s = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(30))).unwrap();
    let body = body.unwrap_or("");
    let req = format!(
        "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
        body.len(),
        body
    );
    s.write_all(req.as_bytes()).unwrap();
    let mut raw = Vec::new();
    s.read_to_end(&mut raw).unwrap();
    let text = String::from_utf8_lossy(&raw).to_string();
    let status: u16 = text
        .split_whitespace()
        .nth(1)
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    let split = text.find("\r\n\r\n").map(|i| i + 4).unwrap_or(text.len());
    // content-length 之外的分块不处理（axum 对小响应给 content-length）
    (status, raw[split..].to_vec())
}

fn http_json(port: u16, method: &str, path: &str, body: Option<Value>) -> (u16, Value) {
    let b = body.map(|v| v.to_string());
    let (status, bytes) = http(port, method, path, b.as_deref());
    let v = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, v)
}

fn find_element<'a>(obs: &'a Value, role: &str, name: &str) -> Option<&'a Value> {
    fn walk<'a>(e: &'a Value, role: &str, name: &str) -> Option<&'a Value> {
        if e.get("role").and_then(Value::as_str) == Some(role)
            && e.get("name").and_then(Value::as_str) == Some(name)
        {
            return Some(e);
        }
        for c in e
            .get("children")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            if let Some(f) = walk(c, role, name) {
                return Some(f);
            }
        }
        None
    }
    for e in obs
        .get("elements")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        if let Some(f) = walk(e, role, name) {
            return Some(f);
        }
    }
    None
}

fn observe(port: u16) -> Value {
    let (status, v) = http_json(port, "POST", "/observe", Some(json!({})));
    assert_eq!(status, 200, "observe 应成功: {v}");
    v
}

fn act(port: u16, frame_id: u64, action: Value) -> Value {
    let (status, v) = http_json(
        port,
        "POST",
        "/act",
        Some(json!({"request_id": "t", "frame_id": frame_id, "action": action})),
    );
    assert_eq!(status, 200, "act 接口应成功: {v}");
    v["result"].clone()
}

/// 被测进程守卫：所有 spawn 均以 process_group(0) 自成进程组，
/// Drop 时整组击杀。只杀本体不够——边缘拉起的 Chromium 是它的
/// 子进程树，测试 panic/失败时若只死边缘，Chromium 会成孤儿带
/// 着投屏/编码循环空转（2026-11-19 实测：漏掉的进程把共享开发
/// 机吃满到 sshd 失联、两次需要人工重启）。整组杀无此问题；
/// 挂靠测试的 Chromium 由测试本体另行 spawn、不在边缘的组里，
/// 不受影响（该测试正要断言它在边缘退出后存活）。
struct EdgeProc(Child);
impl Drop for EdgeProc {
    fn drop(&mut self) {
        let pgid = format!("-{}", self.0.id());
        let _ = Command::new("kill").args(["-KILL", &pgid]).status();
        let _ = self.0.kill();
        let _ = self.0.wait();
        // 兜底清扫：实测有 Chromium 脱离边缘的进程组存活（PGID
        // 与边缘组不符、父进程已变 init）——边缘给浏览器配置目录
        // 的命名是 /tmp/rig-edge-<边缘pid>，按命令行扫掉整棵树。
        let pat = format!("rig-edge-{}([^0-9]|$)", self.0.id());
        let _ = Command::new("pkill").args(["-KILL", "-f", &pat]).status();
    }
}

#[tokio::test]
async fn edge_browser_end_to_end() {
    if std::env::var("RIG_EDGE_IT").is_err() {
        eprintln!("跳过：未设置 RIG_EDGE_IT");
        return;
    }
    let _serial = SERIAL.lock().await;
    // 页面服务（同一端口，127.0.0.1 与 localhost 两个站点 → 跨进程 iframe）
    let listener_a = TcpListener::bind("127.0.0.1:0").unwrap();
    let port_a = listener_a.local_addr().unwrap().port();
    let origin_a = format!("http://127.0.0.1:{port_a}");
    let origin_b = format!("http://localhost:{port_a}");
    let pages = vec![
        ("/login.html".to_string(), LOGIN_HTML.to_string()),
        ("/form.html".to_string(), FORM_HTML.to_string()),
        ("/canvas.html".to_string(), CANVAS_HTML.to_string()),
        ("/iframe.html".to_string(), iframe_html(&origin_b)),
    ];
    let bound = serve(listener_a, pages);
    assert_eq!(bound, port_a);

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let edge_port = listener.local_addr().unwrap().port();
    drop(listener);

    let mut proc = EdgeProc(
        Command::new(env!("CARGO_BIN_EXE_rig-edge-browser"))
            .args([
                "--listen",
                &format!("127.0.0.1:{edge_port}"),
                "--url",
                &format!("{origin_a}/login.html"),
                "--human",
                "--allow-origin",
                &origin_b,
                "--width",
                "900",
                "--height",
                "700",
            ])
            .process_group(0)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("启动 rig-edge-browser"),
    );
    // 等健康
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if let Ok(mut s) = std::net::TcpStream::connect(("127.0.0.1", edge_port)) {
            let _ = s.write_all(b"GET /health HTTP/1.0\r\n\r\n");
            let mut b = String::new();
            if s.read_to_string(&mut b).is_ok() && b.contains("\"ok\":true") {
                break;
            }
        }
        assert!(Instant::now() < deadline, "边缘程序未就绪");
        tokio::time::sleep(Duration::from_millis(300)).await;
    }
    let _ = &mut proc;

    // ── 1. 登录流程 + 脱敏 ───────────────────────────────────
    let o1 = observe(edge_port);
    let obs1 = &o1["observation"];
    let frame1 = obs1["frame_id"].as_u64().unwrap();
    let user_box = find_element(obs1, "textbox", "用户名")
        .expect("应有用户名文本框")
        .clone();
    let pass_box = find_element(obs1, "textbox", "密码")
        .expect("应有密码文本框")
        .clone();
    let user_id = user_box["id"].as_str().unwrap().to_string();
    let pass_id = pass_box["id"].as_str().unwrap().to_string();
    let r = act(
        edge_port,
        frame1,
        json!({"kind": "type", "target": {"by": "element", "id": user_id}, "text": "alice", "submit": false}),
    );
    assert_eq!(r["result"], "done", "输入用户名应成功: {r}");
    let typed = observe_since(edge_port, frame1);
    let base: rig_proto::observation::Observation = serde_json::from_value(obs1.clone()).unwrap();
    let delta: rig_proto::observation::ObservationDelta =
        serde_json::from_value(typed["delta"].clone()).expect("表单输入应支持局部差分");
    let mut applied = base.apply_delta(&delta).unwrap();
    assert_eq!(
        applied.find(&user_id).unwrap().value.as_deref(),
        Some("alice")
    );
    let o2 = observe(edge_port);
    let frame2 = o2["observation"]["frame_id"].as_u64().unwrap();
    applied.frame_id = frame2;
    assert_eq!(
        applied,
        serde_json::from_value(o2["observation"].clone()).unwrap()
    );
    let r = act(
        edge_port,
        frame2,
        json!({"kind": "type", "target": {"by": "element", "id": pass_id}, "text": "hunter2", "submit": false}),
    );
    assert_eq!(r["result"], "done", "输入密码应成功: {r}");
    let o3 = observe(edge_port);
    let whole = o3.to_string();
    assert!(
        !whole.contains("hunter2"),
        "观察结果中不得出现密码明文（脱敏）"
    );
    let frame3 = o3["observation"]["frame_id"].as_u64().unwrap();
    let login_btn = find_element(&o3["observation"], "button", "登录")
        .expect("应有登录按钮")
        .clone();
    let r = act(
        edge_port,
        frame3,
        json!({"kind": "click", "target": {"by": "element", "id": login_btn["id"].as_str().unwrap()}}),
    );
    assert_eq!(r["result"], "done", "点击登录应成功: {r}");
    let o4 = observe(edge_port);
    assert!(
        o4["text"].as_str().unwrap_or("").contains("欢迎 alice"),
        "登录成功后应看到欢迎语，实际渲染：{}",
        o4["text"]
    );

    // ── 2. 跨源 iframe 坐标合成 ──────────────────────────────
    let frame4 = o4["observation"]["frame_id"].as_u64().unwrap();
    let r = act(
        edge_port,
        frame4,
        json!({"kind": "navigate", "url": format!("{origin_a}/iframe.html")}),
    );
    assert_eq!(r["result"], "done", "导航到 iframe 页应成功: {r}");
    tokio::time::sleep(Duration::from_millis(800)).await;
    let o5 = observe(edge_port);
    let obs5 = &o5["observation"];
    let iframe_el = find_element(obs5, "iframe", "内层表单")
        .or_else(|| find_element(obs5, "iframe", ""))
        .expect("应有 iframe 元素")
        .clone();
    let submit = find_element(obs5, "button", "提交")
        .expect("跨源 iframe 内的提交按钮应被观察到")
        .clone();
    assert_eq!(
        find_element(
            &json!({"elements": iframe_el["children"]}),
            "button",
            "提交"
        )
        .map(|button| button["id"].clone()),
        Some(submit["id"].clone()),
        "跨进程帧的提交按钮应挂在宿主 iframe 下"
    );
    let ib = iframe_el["bbox"]
        .as_array()
        .expect("iframe 应有 bbox")
        .clone();
    let sb = submit["bbox"]
        .as_array()
        .expect("提交按钮应有 bbox")
        .clone();
    let (ix1, iy1) = (ib[0].as_f64().unwrap(), ib[1].as_f64().unwrap());
    let (sx1, sy1) = (sb[0].as_f64().unwrap(), sb[1].as_f64().unwrap());
    assert!(
        sx1 >= ix1 - 1.0 && sy1 >= iy1 - 1.0 && sx1 > 10.0,
        "内层按钮坐标应已合成到主帧坐标系：iframe@({ix1},{iy1}) button@({sx1},{sy1})"
    );

    // OOPIF 元素首次使用必須走整頁校驗，Done 回校驗幀；客戶端仍以已交付幀查增量。
    let public_frame = obs5["frame_id"].as_u64().unwrap();
    let submit_id = submit["id"].as_str().unwrap();
    let clicked = act(
        edge_port,
        public_frame,
        json!({"kind":"click", "target":{"by":"element", "id":submit_id}}),
    );
    assert_eq!(
        clicked["result"], "done",
        "跨行程 iframe 點擊應成功: {clicked}"
    );
    let validation_frame = clicked["frame_id"].as_u64().unwrap();
    assert_ne!(
        validation_frame, public_frame,
        "OOPIF 首次點擊應使用完整校驗幀: {clicked}"
    );
    let from_public = observe_since(edge_port, public_frame);
    let after_click: rig_proto::observation::Observation =
        if let Some(delta) = from_public.get("delta") {
            let base: rig_proto::observation::Observation =
                serde_json::from_value(obs5.clone()).unwrap();
            base.apply_delta(&serde_json::from_value(delta.clone()).unwrap())
                .expect("最后交付的观察应能应用差分")
        } else {
            serde_json::from_value(from_public["observation"].clone()).unwrap()
        };
    let mut had_click = false;
    after_click.each_element(&mut |el| had_click |= el.name.as_deref() == Some("点击次数：1"));
    assert!(had_click, "跨行程 iframe 应收到点击: {from_public}");
    let from_unseen_validation = observe_since(edge_port, validation_frame);
    assert!(
        from_unseen_validation.get("observation").is_some()
            && from_unseen_validation.get("delta").is_none(),
        "未交付的 Done 校驗幀不可作為增量基準: {from_unseen_validation}"
    );

    let (status, png) = http(edge_port, "GET", "/screenshot.png", None);
    assert_eq!(status, 200, "跨行程敏感字段的稳定页面应能截图");
    let image = image::load_from_memory(&png).unwrap().to_rgba8();
    let x = ((ix1 + ib[2].as_f64().unwrap()) / 2.0) as u32;
    let y = ((iy1 + ib[3].as_f64().unwrap()) / 2.0) as u32;
    assert_eq!(image.get_pixel(x, y).0, [0, 0, 0, 255]);
    assert_ne!(image.get_pixel(5, 5).0, [0, 0, 0, 255]);

    // ── 3. canvas 页 + 编号截圖 ──────────────────────────────
    let frame5 = obs5["frame_id"].as_u64().unwrap();
    let r = act(
        edge_port,
        frame5,
        json!({"kind": "navigate", "url": format!("{origin_a}/canvas.html")}),
    );
    assert_eq!(r["result"], "done");
    tokio::time::sleep(Duration::from_millis(500)).await;
    let (status, png) = http(edge_port, "GET", "/screenshot.png", None);
    assert_eq!(status, 200, "截圖接口应成功");
    assert!(png.len() > 1000 && &png[..4] == b"\x89PNG", "应返回 PNG 图");

    // ── 4. 人通道 WS 收帧 ────────────────────────────────────
    let (mut ws, _) = tokio_tungstenite::connect_async(format!("ws://127.0.0.1:{edge_port}/ws"))
        .await
        .expect("人通道开启时 WS 应可连接");
    let mut got_frame = false;
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        match tokio::time::timeout(Duration::from_secs(3), ws.next()).await {
            Ok(Some(Ok(msg))) => {
                // 二进制帧：8 字节头（宽/高 u32 小端）+ JPEG（SOI = FF D8）
                if let tokio_tungstenite::tungstenite::Message::Binary(b) = &msg {
                    if b.len() > 10 && b[8] == 0xFF && b[9] == 0xD8 {
                        got_frame = true;
                        break;
                    }
                }
            }
            _ => break,
        }
    }
    assert!(got_frame, "人通道开启时应收到视频帧");
    drop(ws);

    // ── 5. 双通道开关 ────────────────────────────────────────
    let (status, _) = http_json(
        edge_port,
        "POST",
        "/channels",
        Some(json!({"human": false, "ai": false})),
    );
    assert_eq!(status, 200);
    let (status, _) = http_json(edge_port, "POST", "/observe", Some(json!({})));
    assert_eq!(status, 409, "AI 通道关闭时 observe 应被拒");
    let ws_res = tokio_tungstenite::connect_async(format!("ws://127.0.0.1:{edge_port}/ws")).await;
    assert!(ws_res.is_err(), "人通道关闭时 WS 应被拒");
    let (status, v) = http_json(edge_port, "POST", "/channels", Some(json!({"ai": true})));
    assert_eq!(status, 200);
    assert_eq!(v["ai"], true);
    assert_eq!(v["human"], false);
    let (status, _) = http_json(edge_port, "POST", "/observe", Some(json!({})));
    assert_eq!(status, 200, "AI 通道重开后 observe 应恢复");
}

const TEXT_HTML: &str = r#"<!DOCTYPE html><html lang="zh"><head><meta charset="utf-8"><title>文本</title></head>
<body>
<textarea id="ta" aria-label="甲框" rows="4" cols="40"></textarea>
<textarea id="tb" aria-label="乙框" rows="4" cols="40"></textarea>
</body></html>"#;

const TONE_HTML: &str = r#"<!DOCTYPE html><html lang="zh"><head><meta charset="utf-8"><title>音调</title></head>
<body style="background:#336">
<p id="t">正在播放 440Hz</p>
<script>
const ctx = new AudioContext();
const osc = ctx.createOscillator();
const gain = ctx.createGain();
gain.gain.value = 0.15;
osc.frequency.value = 440;
osc.connect(gain); gain.connect(ctx.destination);
osc.start();
setInterval(() => { if (ctx.state !== 'running') ctx.resume(); }, 400);
</script></body></html>"#;

const IMG_HTML: &str = r#"<!DOCTYPE html><html lang="zh"><head><meta charset="utf-8"><title>图片</title></head>
<body>
<div id="zone" aria-label="粘贴区" tabindex="0" style="width:200px;height:80px;border:1px solid #000">点我后粘贴</div>
<p id="report">无图</p>
<button id="copybtn">复制图</button>
<p id="report2"></p>
<script>
const zone = document.getElementById('zone');
zone.addEventListener('paste', async (e) => {
  for (const it of e.clipboardData.items) {
    if (it.type.startsWith('image/')) {
      const bmp = await createImageBitmap(it.getAsFile());
      document.getElementById('report').textContent = 'IMG:' + bmp.width + 'x' + bmp.height;
    }
  }
});
document.getElementById('copybtn').addEventListener('click', async () => {
  const c = document.createElement('canvas'); c.width = 5; c.height = 4;
  const g = c.getContext('2d'); g.fillStyle = '#00c'; g.fillRect(0, 0, 5, 4);
  const blob = await new Promise(r => c.toBlob(r, 'image/png'));
  await navigator.clipboard.write([new ClipboardItem({'image/png': blob})]);
  document.getElementById('report2').textContent = 'copied';
});
</script></body></html>"#;

/// 3x2 红色 PNG（74 字节）
const PNG_3X2: &str = "iVBORw0KGgoAAAANSUhEUgAAAAMAAAACCAYAAACddGYaAAAAEUlEQVR4nGM4ISf3H4YZkDkAmxUME7OZWHkAAAAASUVORK5CYII=";

fn b64_decode(s: &str) -> Vec<u8> {
    let mut table = [255u8; 256];
    for (i, c) in b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/"
        .iter()
        .enumerate()
    {
        table[*c as usize] = i as u8;
    }
    let mut out = Vec::new();
    let mut acc: u32 = 0;
    let mut nbits = 0;
    for &c in s.as_bytes() {
        if c == b'=' {
            break;
        }
        let v = table[c as usize];
        if v == 255 {
            continue;
        }
        acc = (acc << 6) | v as u32;
        nbits += 6;
        if nbits >= 8 {
            nbits -= 8;
            out.push((acc >> nbits) as u8);
        }
    }
    out
}

fn spawn_edge(args: &[String]) -> EdgeProc {
    EdgeProc(
        Command::new(env!("CARGO_BIN_EXE_rig-edge-browser"))
            .args(args)
            .process_group(0)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("启动 rig-edge-browser"),
    )
}

async fn wait_health(edge_port: u16) {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if let Ok(mut s) = std::net::TcpStream::connect(("127.0.0.1", edge_port)) {
            let _ = s.write_all(b"GET /health HTTP/1.0\r\n\r\n");
            let mut b = String::new();
            if s.read_to_string(&mut b).is_ok() && b.contains("\"ok\":true") {
                return;
            }
        }
        assert!(Instant::now() < deadline, "边缘程序未就绪");
        tokio::time::sleep(Duration::from_millis(300)).await;
    }
}

/// 音视频合流（虚拟声卡 + ffmpeg 双版本）与图片复制粘贴双向验证。
#[tokio::test]
async fn edge_av_and_image_clipboard() {
    if std::env::var("RIG_EDGE_IT").is_err() {
        eprintln!("跳过：未设置 RIG_EDGE_IT");
        return;
    }
    let _serial = SERIAL.lock().await;
    let listener_a = TcpListener::bind("127.0.0.1:0").unwrap();
    let port_a = listener_a.local_addr().unwrap().port();
    let origin_a = format!("http://127.0.0.1:{port_a}");
    let bound = serve(
        listener_a,
        vec![
            ("/tone.html".to_string(), TONE_HTML.to_string()),
            ("/img.html".to_string(), IMG_HTML.to_string()),
        ],
    );
    assert_eq!(bound, port_a);
    let free_port = || {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let p = l.local_addr().unwrap().port();
        drop(l);
        p
    };

    // ── 1. 音视频合流：440Hz 音调页 → HEVC/H.264 双版本 HLS ──
    let av_port = free_port();
    let mut proc = spawn_edge(&[
        "--listen".into(),
        format!("127.0.0.1:{av_port}"),
        "--url".into(),
        format!("{origin_a}/tone.html"),
        "--human".into(),
        "--av".into(),
        "--width".into(),
        "900".into(),
        "--height".into(),
        "700".into(),
    ]);
    wait_health(av_port).await;
    let _ = &mut proc;
    let (_, ch) = http_json(av_port, "GET", "/channels", None);
    assert_eq!(ch["av_available"], true, "ffmpeg/PulseAudio 应就位: {ch}");
    assert_eq!(ch["av_wanted"], true, "--av 應保留 AV 意圖: {ch}");
    assert_eq!(ch["av"], false, "沒有播放請求時不應啟動合流管線: {ch}");
    let (status, _) = http(av_port, "GET", "/stream/hevc/not-a-playlist.m3u8", None);
    assert_eq!(status, 404, "無效播放路徑應被拒: {status}");
    let (_, idle) = http_json(av_port, "GET", "/health", None);
    assert_eq!(
        idle["media"]["running"], false,
        "無效播放路徑不可啟動 AV: {idle}"
    );
    // 等 HLS 播放列表与第一个切片
    let deadline = Instant::now() + Duration::from_secs(60);
    let mut playlist = String::new();
    let seg_name = loop {
        let (status, bytes) = http(av_port, "GET", "/stream/hevc/index.m3u8", None);
        if status == 200 {
            playlist = String::from_utf8_lossy(&bytes).to_string();
            if let Some(line) = playlist.lines().find(|l| l.ends_with(".m4s")) {
                break line.trim().to_string();
            }
        }
        assert!(Instant::now() < deadline, "HEVC 播放列表未就绪: {playlist}");
        tokio::time::sleep(Duration::from_millis(500)).await;
    };
    let (_, active) = http_json(av_port, "GET", "/channels", None);
    assert_eq!(
        active["av_wanted"], true,
        "播放請求不應清除 AV 意圖: {active}"
    );
    assert_eq!(
        active["av"], true,
        "有效播放列表請求應啟動合流管線: {active}"
    );
    // H.264 版本与 HEVC 同进程双路输出，首段落盘可能稍晚——同样轮询等待
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let (status, _) = http(av_port, "GET", "/stream/h264/index.m3u8", None);
        if status == 200 {
            break;
        }
        assert!(Instant::now() < deadline, "H.264 兜底版本播放列表未就绪");
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    let (_, init) = http(av_port, "GET", "/stream/hevc/init.mp4", None);
    let (_, seg) = http(av_port, "GET", &format!("/stream/hevc/{seg_name}"), None);
    assert!(
        init.len() > 100 && seg.len() > 1000,
        "初始化段/媒体段应有内容"
    );
    let mut mp4 = init.clone();
    mp4.extend_from_slice(&seg);
    let tmp = std::env::temp_dir().join("rig-av-test.mp4");
    std::fs::write(&tmp, &mp4).unwrap();
    let probe = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-show_entries",
            "stream=codec_name",
            "-of",
            "csv=p=0",
            tmp.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    let codecs = String::from_utf8_lossy(&probe.stdout).to_string();
    assert!(codecs.contains("hevc"), "视频应为 HEVC: {codecs}");
    assert!(codecs.contains("aac"), "音频应为 AAC: {codecs}");
    let vol = Command::new("ffmpeg")
        .args([
            "-i",
            tmp.to_str().unwrap(),
            "-af",
            "volumedetect",
            "-f",
            "null",
            "-",
        ])
        .output()
        .unwrap();
    let vol_text = String::from_utf8_lossy(&vol.stderr).to_string();
    let max_db: f32 = vol_text
        .split("max_volume:")
        .nth(1)
        .and_then(|s| s.split_whitespace().next())
        .and_then(|v| v.parse().ok())
        .unwrap_or(-99.0);
    assert!(
        max_db > -40.0,
        "合流音频不应是静音（440Hz 音调应被虚拟声卡捕获），max_volume={max_db} dB"
    );
    let deadline = Instant::now() + Duration::from_secs(18);
    loop {
        let (_, state) = http_json(av_port, "GET", "/channels", None);
        let (_, health) = http_json(av_port, "GET", "/health", None);
        if state["av"] == false && health["frame_size"] == json!([0, 0]) {
            assert_eq!(state["av_wanted"], true, "播放停止后仍允许后续播放");
            break;
        }
        assert!(
            Instant::now() < deadline,
            "播放需求到期应回收管线与采集: {state} {health}"
        );
        tokio::time::sleep(Duration::from_millis(300)).await;
    }
    drop(proc);

    // ── 2. 图片粘贴（本地→远端）与复制读回（远端→本地）──
    let img_port = free_port();
    let mut proc2 = spawn_edge(&[
        "--listen".into(),
        format!("127.0.0.1:{img_port}"),
        "--url".into(),
        format!("{origin_a}/img.html"),
        "--human".into(),
        "--width".into(),
        "900".into(),
        "--height".into(),
        "700".into(),
    ]);
    wait_health(img_port).await;
    let _ = &mut proc2;
    tokio::time::sleep(Duration::from_millis(600)).await;
    use futures_util::SinkExt;
    let (mut ws, _) = tokio_tungstenite::connect_async(format!("ws://127.0.0.1:{img_port}/ws"))
        .await
        .expect("人通道 WS 应可连接");
    let o1 = observe(img_port);
    let zone = find_element(&o1["observation"], "generic", "粘贴区")
        .expect("应有粘贴区")
        .clone();
    let bb = zone["bbox"].as_array().unwrap().clone();
    // 第五期起 WS 坐标即页面坐标（工具栏改观看页原生 DOM，
    // 不占帧像素、无偏移）
    let (cx, cy) = (
        (bb[0].as_f64().unwrap() + bb[2].as_f64().unwrap()) / 2.0,
        (bb[1].as_f64().unwrap() + bb[3].as_f64().unwrap()) / 2.0,
    );
    // o1 的执行保持窗（2 秒）内人通道手势被遮蔽——先等它结束再点
    tokio::time::sleep(Duration::from_millis(2300)).await;
    for action in ["down", "up"] {
        ws.send(tokio_tungstenite::tungstenite::Message::Text(
            json!({"type": "mouse", "action": action, "x": cx, "y": cy})
                .to_string()
                .into(),
        ))
        .await
        .unwrap();
    }
    tokio::time::sleep(Duration::from_millis(300)).await;
    ws.send(tokio_tungstenite::tungstenite::Message::Text(
        json!({"type": "paste-image", "data": PNG_3X2})
            .to_string()
            .into(),
    ))
    .await
    .unwrap();
    tokio::time::sleep(Duration::from_millis(800)).await;
    let o2 = observe(img_port);
    assert!(
        o2["text"].as_str().unwrap_or("").contains("IMG:3x2"),
        "粘贴的 3x2 图片应被页面收到，实际观察：{}",
        o2["text"]
    );
    // 远端复制图片 → 读回
    let frame2 = o2["observation"]["frame_id"].as_u64().unwrap();
    let btn = find_element(&o2["observation"], "button", "复制图")
        .expect("应有复制图按钮")
        .clone();
    let r = act(
        img_port,
        frame2,
        json!({"kind": "click", "target": {"by": "element", "id": btn["id"].as_str().unwrap()}}),
    );
    assert_eq!(r["result"], "done", "点击复制图应成功: {r}");
    tokio::time::sleep(Duration::from_millis(500)).await;
    ws.send(tokio_tungstenite::tungstenite::Message::Text(
        json!({"type": "clipboard-read-image"}).to_string().into(),
    ))
    .await
    .unwrap();
    let mut img_data: Option<String> = None;
    let deadline = Instant::now() + Duration::from_secs(8);
    while Instant::now() < deadline {
        match tokio::time::timeout(Duration::from_secs(2), ws.next()).await {
            Ok(Some(Ok(msg))) => {
                let t = msg.to_text().unwrap_or("");
                if t.contains("\"clipboard-image\"") {
                    let v: Value = serde_json::from_str(t).unwrap_or(Value::Null);
                    img_data = v["data"].as_str().map(|s| s.to_string());
                    break;
                }
            }
            _ => break,
        }
    }
    let data = img_data.expect("应读回远端复制的图片");
    let bytes = b64_decode(&data);
    assert!(
        bytes.len() > 30 && &bytes[..8] == b"\x89PNG\r\n\x1a\n",
        "读回应为 PNG"
    );
    let w = u32::from_be_bytes(bytes[16..20].try_into().unwrap());
    let h = u32::from_be_bytes(bytes[20..24].try_into().unwrap());
    assert_eq!((w, h), (5, 4), "读回图片应为页面复制的 5x4 PNG");
    drop(ws);
    drop(proc2);
}

/// 向 Chromium 的 DevTools HTTP 端点发 GET，按 Content-Length 读满正文
/// （该端点保活连接，read_to_end 会干等到超时——实测教训）。
fn devtools_get(port: u16, path: &str) -> Option<(u16, String)> {
    let mut s = std::net::TcpStream::connect(("127.0.0.1", port)).ok()?;
    s.set_read_timeout(Some(Duration::from_secs(5))).ok()?;
    s.write_all(format!("GET {path} HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n").as_bytes())
        .ok()?;
    let mut raw = Vec::new();
    let mut buf = [0u8; 4096];
    loop {
        let text = String::from_utf8_lossy(&raw).to_string();
        if let Some(hend) = text.find("\r\n\r\n") {
            let head = &text[..hend];
            let clen: usize = head
                .lines()
                .find_map(|l| {
                    let (k, v) = l.split_once(':')?;
                    if k.trim().eq_ignore_ascii_case("content-length") {
                        v.trim().parse().ok()
                    } else {
                        None
                    }
                })
                .unwrap_or(0);
            if raw.len() >= hend + 4 + clen {
                let status: u16 = head
                    .split_whitespace()
                    .nth(1)
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(0);
                return Some((status, text[hend + 4..].to_string()));
            }
        }
        match s.read(&mut buf) {
            Ok(0) => return None,
            Ok(n) => raw.extend_from_slice(&buf[..n]),
            Err(_) => return None,
        }
    }
}

fn free_port() -> u16 {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let p = l.local_addr().unwrap().port();
    drop(l);
    p
}

/// 发一条带自定义请求头的原始 HTTP 请求，返回状态码。
fn http_status_raw(port: u16, request: &str) -> u16 {
    let mut s = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
    s.write_all(request.as_bytes()).unwrap();
    let mut b = String::new();
    s.read_to_string(&mut b).unwrap_or(0);
    b.split_whitespace()
        .nth(1)
        .and_then(|v| v.parse().ok())
        .unwrap_or(0)
}

/// 直连鉴权：非回环无令牌拒绝启动；带令牌的回环服务全接口查令牌。
#[tokio::test]
async fn edge_token_auth() {
    if std::env::var("RIG_EDGE_IT").is_err() {
        eprintln!("跳过：未设置 RIG_EDGE_IT");
        return;
    }
    let _serial = SERIAL.lock().await;
    // 1) 绑 0.0.0.0 又不给令牌 → 拒绝启动（非零退出）
    let bad_port = free_port();
    let mut bad = spawn_edge(&["--listen".into(), format!("0.0.0.0:{bad_port}")]);
    let deadline = Instant::now() + Duration::from_secs(10);
    let status = loop {
        if let Some(s) = bad.0.try_wait().unwrap() {
            break s;
        }
        assert!(Instant::now() < deadline, "无令牌绑非回环应立即拒绝启动");
        tokio::time::sleep(Duration::from_millis(200)).await;
    };
    assert!(!status.success(), "拒绝启动应是非零退出: {status}");

    // 2) 带令牌启动：/health 开放，其余接口查令牌
    let port = free_port();
    let mut proc = spawn_edge(&[
        "--listen".into(),
        format!("127.0.0.1:{port}"),
        "--token".into(),
        "sekret".into(),
        "--url".into(),
        "about:blank".into(),
    ]);
    wait_health(port).await;
    let _ = &mut proc;
    let (code, _) = http(port, "GET", "/channels", None);
    assert_eq!(code, 401, "无令牌应 401");
    let (code, _) = http(port, "GET", "/channels?token=wrong", None);
    assert_eq!(code, 401, "错令牌应 401");
    let (code, _body) = http(port, "GET", "/channels?token=sekret", None);
    assert_eq!(code, 200, "查询参数令牌应放行");
    let code = http_status_raw(
        port,
        "GET /channels HTTP/1.0\r\nAuthorization: Bearer sekret\r\n\r\n",
    );
    assert_eq!(code, 200, "Bearer 请求头令牌应放行");
    let (code, _) = http(port, "POST", "/observe?token=sekret", Some("{}"));
    assert_eq!(code, 200, "带令牌观察应放行");
    let (code, _) = http(port, "POST", "/observe", Some("{}"));
    assert_eq!(code, 401, "无令牌观察应 401");
    // 3) 观看页子资源：页面本体查令牌，但 /viewer.js 必须免令牌
    // 可达——<script src> 带不上页面 URL 的 ?token=，被 401 时
    // 观看页只剩空壳（第五期上线实踩，LIU 实测全黑）。
    let (code, _) = http(port, "GET", "/", None);
    assert_eq!(code, 401, "无令牌观看页应 401");
    let (code, body) = http(port, "GET", "/viewer.js", None);
    assert_eq!(code, 200, "viewer.js 无令牌应放行（子资源带不上令牌）");
    assert!(
        String::from_utf8_lossy(&body).contains("chrome"),
        "viewer.js 应是观看页脚本"
    );
    drop(proc);
}

fn find_chromium_for_test() -> std::path::PathBuf {
    if let Ok(p) = std::env::var("RIG_CHROMIUM") {
        if !p.is_empty() {
            return std::path::PathBuf::from(p);
        }
    }
    let home = std::env::var("HOME").unwrap();
    let base = std::path::PathBuf::from(home).join(".cache/ms-playwright");
    let mut candidates: Vec<std::path::PathBuf> = vec![];
    for e in std::fs::read_dir(&base).unwrap().flatten() {
        let p = e.path().join("chrome-linux64/chrome");
        if p.exists() {
            candidates.push(p);
        }
    }
    candidates.sort();
    candidates.pop().expect("找不到 Playwright Chromium")
}

/// stdio MCP 全流程 + --cdp-url 挂靠（边缘退出不杀浏览器）。
#[tokio::test]
async fn edge_stdio_mcp_and_attach() {
    if std::env::var("RIG_EDGE_IT").is_err() {
        eprintln!("跳过：未设置 RIG_EDGE_IT");
        return;
    }
    let _serial = SERIAL.lock().await;
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let origin = format!("http://127.0.0.1:{port}");
    let bound = serve(listener, vec![("/login.html".into(), LOGIN_HTML.into())]);
    assert_eq!(bound, port);

    // ── A. stdio MCP：initialize → tools/list → observe → act → screenshot ──
    let mut child = EdgeProc(
        Command::new(env!("CARGO_BIN_EXE_rig-edge-browser"))
            .args(["--stdio", "--url", &format!("{origin}/login.html")])
            .process_group(0)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("启动 stdio 模式"),
    );
    let mut w = child.0.stdin.take().unwrap();
    let (tx, rx) = std::sync::mpsc::channel::<String>();
    let mut reader = std::io::BufReader::new(child.0.stdout.take().unwrap());
    std::thread::spawn(move || {
        use std::io::BufRead;
        let mut line = String::new();
        loop {
            line.clear();
            match reader.read_line(&mut line) {
                Ok(0) | Err(_) => break,
                Ok(_) => {
                    if tx.send(line.clone()).is_err() {
                        break;
                    }
                }
            }
        }
    });
    let mut next_id = 0;
    let mut rpc = |w: &mut std::process::ChildStdin, method: &str, params: Value| -> Value {
        next_id += 1;
        let req = json!({"jsonrpc": "2.0", "id": next_id, "method": method, "params": params});
        writeln!(w, "{req}").unwrap();
        w.flush().unwrap();
        let line = rx
            .recv_timeout(Duration::from_secs(25))
            .expect("MCP 响应超时");
        serde_json::from_str(&line).unwrap()
    };
    let init = rpc(
        &mut w,
        "initialize",
        json!({"protocolVersion": "2025-03-26", "capabilities": {}, "clientInfo": {"name": "it", "version": "0"}}),
    );
    assert_eq!(init["result"]["serverInfo"]["name"], "rig-edge-browser");
    writeln!(
        w,
        "{}",
        json!({"jsonrpc": "2.0", "method": "notifications/initialized"})
    )
    .unwrap();
    w.flush().unwrap();
    let list = rpc(&mut w, "tools/list", json!({}));
    let tools = list["result"]["tools"].as_array().unwrap();
    assert_eq!(tools.len(), 3, "应有 observe/act/screenshot 三件工具");
    // 页面刚导航完时 AX 树可能只建了一半（标题先行、表单随后）——
    // 轮询观察直到用户框出现，与真实客户端的就绪等待一致。
    let mut obs_json = Value::Null;
    let mut user_el = Value::Null;
    for _ in 0..16 {
        let o = rpc(
            &mut w,
            "tools/call",
            json!({"name": "observe", "arguments": {}}),
        );
        obs_json =
            serde_json::from_str(o["result"]["content"][1]["text"].as_str().unwrap()).unwrap();
        if let Some(el) = find_element(&obs_json, "textbox", "用户名") {
            let text0 = o["result"]["content"][0]["text"].as_str().unwrap();
            assert!(text0.contains("登录"), "observe 应看到登录页: {text0}");
            user_el = el.clone();
            break;
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    assert!(user_el.is_object(), "等不到用户框，观察：{obs_json}");
    let frame = obs_json["frame_id"].as_u64().unwrap();
    let r = rpc(
        &mut w,
        "tools/call",
        json!({"name": "act", "arguments": {"frame_id": frame, "action": {"kind": "type", "target": {"by": "element", "id": user_el["id"].as_str().unwrap()}, "text": "alice", "submit": false}}}),
    );
    let rtext = r["result"]["content"][0]["text"].as_str().unwrap();
    assert!(rtext.contains("done"), "act type 应成功: {rtext}");
    let rejected = rpc(
        &mut w,
        "tools/call",
        json!({"name": "act", "arguments": {"frame_id": frame + 1000, "action": {"kind": "type", "target": {"by": "element", "id": user_el["id"]}, "text": "should-not-appear", "submit": false}}}),
    );
    let content = rejected["result"]["content"].as_array().unwrap();
    let result: Value = serde_json::from_str(content[0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(result["result"], "rejected");
    assert_eq!(result["reason"]["code"], "stale_frame");
    let fresh: Value = serde_json::from_str(content[2]["text"].as_str().unwrap()).unwrap();
    let user = find_element(&fresh, "textbox", "用户名").expect("拒绝回包应附结构化新观察");
    assert_eq!(user["value"], "alice", "过期动作不应执行输入");
    let shot = rpc(
        &mut w,
        "tools/call",
        json!({"name": "screenshot", "arguments": {}}),
    );
    assert_eq!(shot["result"]["content"][0]["type"], "image");
    assert!(
        shot["result"]["content"][0]["data"]
            .as_str()
            .unwrap()
            .starts_with("iVBOR"),
        "截图应是 PNG base64"
    );
    drop(w);
    let deadline = Instant::now() + Duration::from_secs(10);
    let status = loop {
        if let Some(s) = child.0.try_wait().unwrap() {
            break s;
        }
        assert!(Instant::now() < deadline, "stdin 关闭后边缘应自行退出");
        std::thread::sleep(Duration::from_millis(200));
    };
    assert!(status.success(), "stdio 模式应正常退出: {status}");

    // ── B. --cdp-url 挂靠：边缘连现成 Chromium，退出不杀它 ──
    let exe = find_chromium_for_test();
    let cdp_port = free_port();
    // 配置目录每次唯一：固定目录会让上次残留的 Chromium 实例接管、
    // 新进程直接退出，调试端口永远不开（实测教训）
    let profile = format!("/tmp/rig-attach-it-{cdp_port}");
    let mut chrome = EdgeProc(
        Command::new(&exe)
            .args([
                "--headless=new",
                "--no-sandbox",
                "--disable-gpu",
                &format!("--remote-debugging-port={cdp_port}"),
                &format!("--user-data-dir={profile}"),
                "about:blank",
            ])
            .process_group(0)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("手动启动 Chromium"),
    );
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        if let Some((code, _)) = devtools_get(cdp_port, "/json/version") {
            if code == 200 {
                break;
            }
        }
        assert!(Instant::now() < deadline, "Chromium 调试端口未就绪");
        std::thread::sleep(Duration::from_millis(300));
    }
    let edge_port = free_port();
    let edge = spawn_edge(&[
        "--listen".into(),
        format!("127.0.0.1:{edge_port}"),
        "--cdp-url".into(),
        format!("http://127.0.0.1:{cdp_port}"),
        "--url".into(),
        format!("{origin}/login.html"),
    ]);
    wait_health(edge_port).await;
    let o = observe(edge_port);
    assert!(
        o["text"].as_str().unwrap_or("").contains("登录"),
        "挂靠模式应能观察页面"
    );
    drop(edge);
    std::thread::sleep(Duration::from_millis(600));
    let alive = devtools_get(cdp_port, "/json/version").map(|(c, _)| c);
    assert_eq!(alive, Some(200), "边缘退出后挂靠的 Chromium 应存活");
    let _ = chrome.0.kill();
    let _ = chrome.0.wait();
    let _ = std::fs::remove_dir_all(&profile);
}

fn elem_value(obs: &Value, role: &str, name: &str) -> String {
    find_element(obs, role, name)
        .and_then(|e| e.get("value"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string()
}

/// 文本输入 / 组合键 / 剪贴板 / 输入法成品 / 人通道修饰键的端到端验证。
#[tokio::test]
async fn edge_text_keys_clipboard() {
    if std::env::var("RIG_EDGE_IT").is_err() {
        eprintln!("跳过：未设置 RIG_EDGE_IT");
        return;
    }
    let _serial = SERIAL.lock().await;
    let listener_a = TcpListener::bind("127.0.0.1:0").unwrap();
    let port_a = listener_a.local_addr().unwrap().port();
    let origin_a = format!("http://127.0.0.1:{port_a}");
    let bound = serve(
        listener_a,
        vec![("/text.html".to_string(), TEXT_HTML.to_string())],
    );
    assert_eq!(bound, port_a);

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let edge_port = listener.local_addr().unwrap().port();
    drop(listener);
    let mut proc = EdgeProc(
        Command::new(env!("CARGO_BIN_EXE_rig-edge-browser"))
            .args([
                "--listen",
                &format!("127.0.0.1:{edge_port}"),
                "--url",
                &format!("{origin_a}/text.html"),
                "--human",
                "--width",
                "900",
                "--height",
                "700",
            ])
            .process_group(0)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("启动 rig-edge-browser"),
    );
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if let Ok(mut s) = std::net::TcpStream::connect(("127.0.0.1", edge_port)) {
            let _ = s.write_all(b"GET /health HTTP/1.0\r\n\r\n");
            let mut b = String::new();
            if s.read_to_string(&mut b).is_ok() && b.contains("\"ok\":true") {
                break;
            }
        }
        assert!(Instant::now() < deadline, "边缘程序未就绪");
        tokio::time::sleep(Duration::from_millis(300)).await;
    }
    let _ = &mut proc;
    tokio::time::sleep(Duration::from_millis(600)).await;

    // ── 1. AI 通道输入中日文（insertText 直插，无需 IME 组字）──
    let o1 = observe(edge_port);
    let frame1 = o1["observation"]["frame_id"].as_u64().unwrap();
    let ta = find_element(&o1["observation"], "textbox", "甲框")
        .expect("应有甲框")
        .clone();
    let ta_id = ta["id"].as_str().unwrap().to_string();
    let cjk = "你好世界 こんにちは";
    let r = act(
        edge_port,
        frame1,
        json!({"kind": "type", "target": {"by": "element", "id": ta_id}, "text": cjk, "submit": false}),
    );
    assert_eq!(r["result"], "done", "输入中日文应成功: {r}");
    let o2 = observe(edge_port);
    assert!(
        elem_value(&o2["observation"], "textbox", "甲框").contains("你好世界"),
        "甲框应含中文，实际观察：{}",
        o2["text"]
    );

    // ── 1b. 幂等（R-06）：同一请求（同 request_id + 同内容）
    //        重投应回缓存结果，不得二次输入 ──
    let r = act(
        edge_port,
        frame1,
        json!({"kind": "type", "target": {"by": "element", "id": ta_id}, "text": cjk, "submit": false}),
    );
    assert_eq!(r["result"], "done", "重复请求应回缓存的 done: {r}");
    let o2dup = observe(edge_port);
    assert_eq!(
        elem_value(&o2dup["observation"], "textbox", "甲框"),
        cjk,
        "重复请求不得二次输入（值应恰好一份）"
    );

    // ── 2. 页内复制粘贴：Ctrl+A / Ctrl+C → 点乙框 → Ctrl+V ──
    let frame2 = o2["observation"]["frame_id"].as_u64().unwrap();
    for keys in ["Control+A", "Control+C"] {
        let r = act(edge_port, frame2, json!({"kind": "key", "key": keys}));
        assert_eq!(r["result"], "done", "{keys} 应成功: {r}");
    }
    // 契约纪律：按键动作已推进帧号，引用元素前先重新观察取当前帧
    let o2b = observe(edge_port);
    let frame2b = o2b["observation"]["frame_id"].as_u64().unwrap();
    let tb = find_element(&o2b["observation"], "textbox", "乙框")
        .expect("应有乙框")
        .clone();
    let r = act(
        edge_port,
        frame2b,
        json!({"kind": "click", "target": {"by": "element", "id": tb["id"].as_str().unwrap()}}),
    );
    assert_eq!(r["result"], "done", "点击乙框应成功: {r}");
    let o3 = observe(edge_port);
    let frame3 = o3["observation"]["frame_id"].as_u64().unwrap();
    let r = act(
        edge_port,
        frame3,
        json!({"kind": "key", "key": "Control+V"}),
    );
    assert_eq!(r["result"], "done", "Ctrl+V 应成功: {r}");
    let o4 = observe(edge_port);
    assert!(
        elem_value(&o4["observation"], "textbox", "乙框").contains("你好世界"),
        "乙框应粘贴到中文（页内剪贴板），实际观察：{}",
        o4["text"]
    );

    // ── 3. 人通道：点击聚焦 → 输入法成品整段直插 ──
    let (mut ws, _) = tokio_tungstenite::connect_async(format!("ws://127.0.0.1:{edge_port}/ws"))
        .await
        .expect("人通道 WS 应可连接");
    use futures_util::SinkExt;
    // §2 的 observe/act 后有 2 秒执行保持窗（§7.1），窗内人通道手势
    // 被遮蔽——先等保持窗结束再点
    tokio::time::sleep(Duration::from_millis(2300)).await;
    let bb = ta["bbox"].as_array().unwrap().clone();
    // 第五期起 WS 坐标即页面坐标（无工具栏偏移）
    let (cx, cy) = (
        (bb[0].as_f64().unwrap() + bb[2].as_f64().unwrap()) / 2.0,
        (bb[1].as_f64().unwrap() + bb[3].as_f64().unwrap()) / 2.0,
    );
    for action in ["down", "up"] {
        ws.send(tokio_tungstenite::tungstenite::Message::Text(
            json!({"type": "mouse", "action": action, "x": cx, "y": cy})
                .to_string()
                .into(),
        ))
        .await
        .unwrap();
    }
    tokio::time::sleep(Duration::from_millis(300)).await;
    ws.send(tokio_tungstenite::tungstenite::Message::Text(
        json!({"type": "text", "text": "中文IMEテスト"})
            .to_string()
            .into(),
    ))
    .await
    .unwrap();
    tokio::time::sleep(Duration::from_millis(500)).await;
    let o5 = observe(edge_port);
    assert!(
        elem_value(&o5["observation"], "textbox", "甲框").contains("中文IMEテスト"),
        "输入法成品应整段进入甲框，实际观察：{}",
        o5["text"]
    );

    // ── 4. 人通道修饰键：Ctrl+A 全选 + Backspace 清空 ──
    // 同样先等 o5 的执行保持窗结束，键击才不被遮蔽
    tokio::time::sleep(Duration::from_millis(2300)).await;
    for (action, key, code, modifiers) in [
        ("down", "a", "KeyA", 2),
        ("up", "a", "KeyA", 2),
        ("down", "Backspace", "Backspace", 0),
        ("up", "Backspace", "Backspace", 0),
    ] {
        ws.send(tokio_tungstenite::tungstenite::Message::Text(
            json!({"type": "key", "action": action, "key": key, "code": code, "modifiers": modifiers})
                .to_string()
                .into(),
        ))
        .await
        .unwrap();
        tokio::time::sleep(Duration::from_millis(80)).await;
    }
    tokio::time::sleep(Duration::from_millis(400)).await;
    let o6 = observe(edge_port);
    let v = elem_value(&o6["observation"], "textbox", "甲框");
    assert!(
        v.is_empty(),
        "Ctrl+A + Backspace 后甲框应为空（人通道修饰键），实际值：{v:?}，观察：{}",
        o6["text"]
    );
    drop(ws);
}

const A_HTML: &str = r#"<!DOCTYPE html><html lang="zh"><head><meta charset="utf-8"><title>A页标题</title></head>
<body><h1>A页内容</h1><p>alpha</p></body></html>"#;

const B_HTML: &str = r#"<!DOCTYPE html><html lang="zh"><head><meta charset="utf-8"><title>B页标题</title></head>
<body><h1>B页内容</h1><p>beta</p></body></html>"#;

const BLOCK_HTML: &str = r#"<!DOCTYPE html><html lang="zh"><head><meta charset="utf-8"><title>冻结实验室</title></head>
<body>
<button id="fz" style="position:absolute;left:10px;top:10px;width:200px;height:60px">冻结</button>
<button id="tg" style="position:absolute;left:10px;top:120px;width:200px;height:60px">目标</button>
<p id="st" style="margin-top:220px">待命</p>
<script>
document.getElementById('fz').addEventListener('click', () => {
  const t = Date.now();
  while (Date.now() - t < 5000) {}
  document.getElementById('st').textContent = '已解冻';
});
document.getElementById('tg').addEventListener('click', () => {
  document.getElementById('st').textContent = '已点击';
});
</script>
</body></html>"#;

/// chrome UI（第五期：chrome 命令/状态推送/纯页面帧/标签开闭）、Point 校验
/// （越界/陈旧帧）、交互模型（执行期遮蔽 + 强制接管 Aborted）。
#[tokio::test]
async fn edge_chrome_ui_and_takeover() {
    if std::env::var("RIG_EDGE_IT").is_err() {
        eprintln!("跳过：未设置 RIG_EDGE_IT");
        return;
    }
    let _serial = SERIAL.lock().await;
    let listener_a = TcpListener::bind("127.0.0.1:0").unwrap();
    let port_a = listener_a.local_addr().unwrap().port();
    let origin_a = format!("http://127.0.0.1:{port_a}");
    serve(
        listener_a,
        vec![
            ("/a.html".to_string(), A_HTML.to_string()),
            ("/b.html".to_string(), B_HTML.to_string()),
            ("/block.html".to_string(), BLOCK_HTML.to_string()),
        ],
    );
    let edge_port = free_port();
    let mut proc = spawn_edge(&[
        "--listen".into(),
        format!("127.0.0.1:{edge_port}"),
        "--url".into(),
        format!("{origin_a}/a.html"),
        "--human".into(),
        "--width".into(),
        "900".into(),
        "--height".into(),
        "700".into(),
    ]);
    wait_health(edge_port).await;
    let _ = &mut proc;
    tokio::time::sleep(Duration::from_millis(800)).await;

    // ── 1. Point 校验（B4/R-06） ────────────────────────────
    let o1 = observe(edge_port);
    let f1 = o1["observation"]["frame_id"].as_u64().unwrap();
    assert!(o1["text"].as_str().unwrap_or("").contains("A页内容"));
    let r = act(
        edge_port,
        f1,
        json!({"kind": "click", "target": {"by": "point", "x": 850, "y": 650}}),
    );
    assert_eq!(r["result"], "done", "视口内 Point 点击应成功: {r}");
    let o2 = observe(edge_port);
    let f2 = o2["observation"]["frame_id"].as_u64().unwrap();
    let r = act(
        edge_port,
        f2,
        json!({"kind": "click", "target": {"by": "point", "x": 950, "y": 650}}),
    );
    assert_eq!(r["result"], "rejected", "越界 Point 应被拒: {r}");
    assert_eq!(r["reason"]["code"], "out_of_bounds", "应为越界拒绝: {r}");
    let r = act(
        edge_port,
        f1,
        json!({"kind": "click", "target": {"by": "point", "x": 100, "y": 100}}),
    );
    assert_eq!(r["result"], "rejected", "陈旧帧 Point 应被拒: {r}");
    assert_eq!(r["reason"]["code"], "stale_frame", "应为陈旧帧拒绝: {r}");
    // 被拒时程序自己附最新一帧（AI 不必再补 observe）
    let (_, full) = http_json(
        edge_port,
        "POST",
        "/act",
        Some(
            json!({"request_id": "stale-fresh", "frame_id": f1, "action":
            {"kind": "click", "target": {"by": "point", "x": 100, "y": 100}}}),
        ),
    );
    // f1 仍在历史池 → 回增量（局部刷新）；带的是最新状态
    assert!(
        full["delta"]["frame_id"].as_u64().unwrap_or(0) >= f2
            || full["observation"]["frame_id"].as_u64().unwrap_or(0) >= f2,
        "陈旧拒绝应附带最新状态（delta 或整帧）: {full}"
    );
    assert!(full["text"].as_str().is_some_and(|t| !t.is_empty()));

    // ── 2. chrome 命令与状态推送（第五期：原生工具栏） ──
    // 注意：每次 HTTP observe/act 后有 2 秒执行保持窗（§7.1），窗内
    // 观看端页面手势会被遮蔽并弹接管提示——页面点击前先等保持窗
    // 结束（chrome 命令是工具栏操作，不受遮蔽）。
    use futures_util::SinkExt;
    let (mut ws, _) = tokio_tungstenite::connect_async(format!("ws://127.0.0.1:{edge_port}/ws"))
        .await
        .expect("人通道 WS 应可连接");
    // 等一帧并核对帧尺寸（第五期：纯页面帧，高恰为页面高）
    let mut frame_h = 0u64;
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline && frame_h == 0 {
        if let Ok(Some(Ok(tokio_tungstenite::tungstenite::Message::Binary(b)))) =
            tokio::time::timeout(Duration::from_secs(3), ws.next()).await
        {
            // 二进制帧头：宽/高为 u32 小端
            if b.len() > 8 {
                let w = u32::from_le_bytes(b[0..4].try_into().unwrap()) as u64;
                assert_eq!(w, 900, "帧宽应为页面宽");
                frame_h = u32::from_le_bytes(b[4..8].try_into().unwrap()) as u64;
            }
        }
    }
    assert_eq!(
        frame_h, 700,
        "帧高应恰为页面高 700（工具栏不再合成进帧），实际 {frame_h}"
    );
    macro_rules! ws_send {
        ($v:expr) => {
            ws.send(tokio_tungstenite::tungstenite::Message::Text(
                $v.to_string().into(),
            ))
            .await
            .unwrap()
        };
    }
    macro_rules! click {
        ($x:expr, $y:expr) => {{
            ws_send!(json!({"type": "mouse", "action": "down", "x": $x, "y": $y}));
            tokio::time::sleep(Duration::from_millis(90)).await;
            ws_send!(json!({"type": "mouse", "action": "up", "x": $x, "y": $y}));
            tokio::time::sleep(Duration::from_millis(120)).await;
        }};
    }
    // 等一个满足断言的 chrome-state 通知（工具栏状态经 WS 文本
    // 推送断言，不再靠帧高/像素推断）
    macro_rules! wait_state {
        ($pred:expr, $what:expr) => {{
            let deadline = Instant::now() + Duration::from_secs(8);
            let mut ok = false;
            while Instant::now() < deadline && !ok {
                if let Ok(Some(Ok(msg))) =
                    tokio::time::timeout(Duration::from_secs(2), ws.next()).await
                {
                    if let Ok(t) = msg.to_text() {
                        if t.contains("\"chrome-state\"") && ($pred)(t) {
                            ok = true;
                        }
                    }
                }
            }
            assert!(ok, "应收到 chrome-state：{}", $what);
        }};
    }
    // 连接即应收到初始 chrome-state（活动标签为 A 页）
    wait_state!(|t: &str| t.contains("/a.html"), "初始状态应含 A 页标签");
    // 地址栏导航（chrome 命令）→ B 页
    tokio::time::sleep(Duration::from_millis(2300)).await; // 等保持窗结束
    ws_send!(json!({"type": "chrome", "action": "navigate", "url": format!("{origin_a}/b.html")}));
    tokio::time::sleep(Duration::from_millis(1200)).await;
    let o = observe(edge_port);
    assert!(
        o["text"].as_str().unwrap_or("").contains("B页内容"),
        "地址栏导航后应到 B 页，实际：{}",
        o["text"]
    );
    // 后退命令 → 回 A 页
    tokio::time::sleep(Duration::from_millis(2300)).await;
    ws_send!(json!({"type": "chrome", "action": "back"}));
    tokio::time::sleep(Duration::from_millis(1200)).await;
    let o = observe(edge_port);
    assert!(
        o["text"].as_str().unwrap_or("").contains("A页内容"),
        "后退后应回 A 页，实际：{}",
        o["text"]
    );
    // 页内寻找：open → 状态 find 非空；text → info 带计数；close → 回 null
    ws_send!(json!({"type": "chrome", "action": "find-open"}));
    wait_state!(|t: &str| t.contains("\"find\":{"), "寻找开启后 find 应非空");
    ws_send!(json!({"type": "chrome", "action": "find-text", "text": "A页内容"}));
    wait_state!(|t: &str| t.contains("处"), "寻找计数应出现在状态里");
    ws_send!(json!({"type": "chrome", "action": "find-close"}));
    wait_state!(
        |t: &str| t.contains("\"find\":null"),
        "寻找关闭后 find 应回 null"
    );
    // 新建标签 → 状态里两个标签且活动为新标签；观察为空白页
    ws_send!(json!({"type": "chrome", "action": "new-tab"}));
    wait_state!(
        |t: &str| t.matches("\"url\":").count() >= 2 && t.contains("\"active\":1"),
        "新建标签后应有两标签且活动为第二个"
    );
    tokio::time::sleep(Duration::from_millis(1000)).await;
    let o = observe(edge_port);
    assert!(
        !o["text"].as_str().unwrap_or("").contains("A页内容"),
        "新建标签后观察应为空白页，实际：{}",
        o["text"]
    );
    // 关掉新标签（index 1）→ 回到 A 页标签
    ws_send!(json!({"type": "chrome", "action": "close-tab", "index": 1}));
    wait_state!(
        |t: &str| t.matches("\"url\":").count() == 1,
        "关闭新标签后应剩一个标签"
    );
    tokio::time::sleep(Duration::from_millis(1000)).await;
    let o = observe(edge_port);
    assert!(
        o["text"].as_str().unwrap_or("").contains("A页内容"),
        "关闭新标签后应回到 A 页，实际：{}",
        o["text"]
    );

    // ── 3. 强制接管：冻结页让 act 的观察卡住 → 执行期手势弹提示
    //        → 接管 → act 返回 Aborted 且点击从未落地 ──────────
    let o3 = observe(edge_port);
    let f3 = o3["observation"]["frame_id"].as_u64().unwrap();
    let r = act(
        edge_port,
        f3,
        json!({"kind": "navigate", "url": format!("{origin_a}/block.html")}),
    );
    assert_eq!(r["result"], "done", "导航到冻结实验室应成功: {r}");
    tokio::time::sleep(Duration::from_millis(600)).await;
    let o4 = observe(edge_port);
    let f4 = o4["observation"]["frame_id"].as_u64().unwrap();
    let tg = find_element(&o4["observation"], "button", "目标")
        .expect("应有目标按钮")
        .clone();
    let tg_id = tg["id"].as_str().unwrap().to_string();
    // 人通道点「冻结」（页面坐标即 WS 坐标，第五期无偏移）；
    // 先等 o4 的保持窗结束，这一击才不会被遮蔽
    tokio::time::sleep(Duration::from_millis(2300)).await;
    click!(110.0, 40.0);
    // AI 在另一线程点「目标」：其内部观察会被冻结的主线程卡住
    let (tx, rx) = std::sync::mpsc::channel();
    let tg_id_thread = tg_id.clone();
    std::thread::spawn(move || {
        let r = act(
            edge_port,
            f4,
            json!({"kind": "click", "target": {"by": "element", "id": tg_id_thread}}),
        );
        let _ = tx.send(r);
    });
    tokio::time::sleep(Duration::from_millis(900)).await;
    // 执行期在页面区按下 → 应收到接管提示（帧消息一并读掉）
    ws_send!(json!({"type": "mouse", "action": "down", "x": 600, "y": 400}));
    let mut prompted = false;
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline && !prompted {
        if let Ok(Some(Ok(msg))) = tokio::time::timeout(Duration::from_secs(2), ws.next()).await {
            if msg.to_text().unwrap_or("").contains("takeover-prompt") {
                prompted = true;
            }
        }
    }
    assert!(prompted, "执行期页面手势应弹出强制接管提示");
    ws_send!(json!({"type": "takeover", "yes": true}));
    let r = rx
        .recv_timeout(Duration::from_secs(20))
        .expect("被接管的 act 应返回");
    assert_eq!(r["result"], "aborted", "接管后 act 应为 Aborted: {r}");
    assert_eq!(r["reason"], "user_takeover", "中断原因应为用户接管: {r}");
    // 接管闩锁（第一期）：未重新 observe 前的新 act 直接被拒
    let r = act(
        edge_port,
        f4,
        json!({"kind": "click", "target": {"by": "element", "id": tg_id}}),
    );
    assert_eq!(r["result"], "aborted", "闩锁中 act 应被拒: {r}");
    // 用接管前的观察帧恢复同步，验证目标点击从未执行。
    tokio::time::sleep(Duration::from_millis(600)).await;
    let o5 = observe_since(edge_port, f4);
    let current = if let Some(delta) = o5.get("delta") {
        let base: rig_proto::observation::Observation =
            serde_json::from_value(o4["observation"].clone()).unwrap();
        serde_json::to_value(
            base.apply_delta(&serde_json::from_value(delta.clone()).unwrap())
                .expect("接管后的差分应能应用"),
        )
        .unwrap()
    } else {
        o5["observation"].clone()
    };
    let text5 = current.to_string();
    assert!(text5.contains("已解冻"), "页面应已解冻，实际：{text5}");
    assert!(
        !text5.contains("已点击"),
        "被接管的点击不应落地，实际：{text5}"
    );
    // o5 的 observe 已重新同步 → 闩锁解除，再 act 应放行并落地
    let f5 = current["frame_id"].as_u64().unwrap();
    let tg2 = find_element(&current, "button", "目标")
        .expect("应有目标按钮")
        .clone();
    let tg2_id = tg2["id"].as_str().unwrap().to_string();
    let r = act(
        edge_port,
        f5,
        json!({"kind": "click", "target": {"by": "element", "id": tg2_id}}),
    );
    assert_eq!(r["result"], "done", "observe 后 act 应放行: {r}");
    let o6 = observe(edge_port);
    let text6 = o6["text"].as_str().unwrap_or("");
    assert!(
        text6.contains("已点击"),
        "恢复后的点击应落地，实际：{text6}"
    );

    let f6 = o6["observation"]["frame_id"].as_u64().unwrap();
    ws_send!(json!({"type":"resize", "w":800, "h":600}));
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut since = f6;
    loop {
        let response = observe_since(edge_port, since);
        if response["observation"]["space"]["width"] == 800.0 {
            assert!(response.get("delta").is_none(), "坐标空间变化应回整帧");
            assert_eq!(response["observation"]["space"]["height"], 600.0);
            break;
        }
        let frame = response
            .get("delta")
            .or_else(|| response.get("observation"))
            .unwrap();
        since = frame["frame_id"].as_u64().unwrap();
        assert!(Instant::now() < deadline, "浏览器应应用新视口: {response}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    drop(ws);
}

const DOM_HTML: &str = r#"<!DOCTYPE html><html lang="zh"><head><meta charset="utf-8"><title>DOM测试页</title>
<style>body{margin:0} #name{position:absolute;left:10px;top:10px;width:200px;height:30px}
#add{position:absolute;left:10px;top:60px;width:120px;height:30px} .tall{height:3000px}</style></head>
<body>
<input id="name" type="text">
<button id="add" onclick="var d=document.createElement('div');d.className='added';d.textContent='新增节点';document.body.appendChild(d);">添加</button>
<canvas id="cv1" width="120" height="60" style="position:absolute;left:10px;top:110px"></canvas>
<script>var g=document.getElementById('cv1').getContext('2d');g.fillStyle='#d33';g.fillRect(0,0,120,60);g.fillStyle='#fff';g.font='20px sans-serif';g.fillText('画布',10,35);</script>
<iframe src="/frame.html" width="320" height="130" style="position:absolute;left:10px;top:200px"></iframe>
<div class="tall"></div>
</body></html>"#;

const FRAME_HTML: &str = r#"<!DOCTYPE html><html lang="zh"><head><meta charset="utf-8"><title>内嵌页</title></head>
<body>
<input id="fin" type="text">
<script>
setTimeout(() => {
  var p = document.createElement('p');
  p.id = 'late';
  p.textContent = '迟到节点';
  document.body.appendChild(p);
}, 900);
</script>
</body></html>"#;

/// 在序列化树里找第一个 IFRAME 节点（含其 f 内容子树）
fn tree_find_iframe(node: &Value) -> Option<Value> {
    if node["g"] == "IFRAME" {
        return Some(node.clone());
    }
    for c in node["c"].as_array().into_iter().flatten() {
        if let Some(f) = tree_find_iframe(c) {
            return Some(f);
        }
    }
    None
}

/// 序列化树（c 子节点 + iframe 的 f）里是否存在属性 id 为 id 的元素
fn tree_has_attr_id(node: &Value, id: &str) -> bool {
    if node["a"]["id"].as_str() == Some(id) {
        return true;
    }
    for c in node["c"].as_array().into_iter().flatten() {
        if tree_has_attr_id(c, id) {
            return true;
        }
    }
    if let Some(f) = node.get("f") {
        if tree_has_attr_id(f, id) {
            return true;
        }
    }
    false
}

/// 在序列化树里找第一个 CANVAS 节点的 id
fn tree_find_canvas(node: &Value) -> Option<u64> {
    if node["g"] == "CANVAS" {
        return node["i"].as_u64();
    }
    for c in node["c"].as_array().into_iter().flatten() {
        if let Some(i) = tree_find_canvas(c) {
            return Some(i);
        }
    }
    None
}

type WsStream =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

/// 读 WS 直到出现指定 type 的文本消息（跳过二进制帧与其他消息）
async fn next_dom(ws: &mut WsStream, want: &str, within: Duration) -> Value {
    let deadline = Instant::now() + within;
    while Instant::now() < deadline {
        match tokio::time::timeout(Duration::from_secs(3), ws.next()).await {
            Ok(Some(Ok(tokio_tungstenite::tungstenite::Message::Text(t)))) => {
                if let Ok(v) = serde_json::from_str::<Value>(&t) {
                    if v["type"] == want {
                        return v;
                    }
                }
            }
            Ok(Some(Ok(_))) => {}
            _ => {}
        }
    }
    panic!("等待 {want} 超时");
}

/// 收集 dom-delta 的 (seq, op) 直到谓词满足或超时
async fn collect_deltas_until<F: Fn(&[(u64, Value)]) -> bool>(
    ws: &mut WsStream,
    pred: F,
    within: Duration,
) -> Vec<(u64, Value)> {
    let mut got: Vec<(u64, Value)> = Vec::new();
    let deadline = Instant::now() + within;
    while Instant::now() < deadline && !pred(&got) {
        match tokio::time::timeout(Duration::from_secs(3), ws.next()).await {
            Ok(Some(Ok(tokio_tungstenite::tungstenite::Message::Text(t)))) => {
                if let Ok(v) = serde_json::from_str::<Value>(&t) {
                    if v["type"] == "dom-delta" {
                        let seq = v["seq"].as_u64().unwrap_or(0);
                        if let Some(arr) = v["ops"].as_array() {
                            got.extend(arr.iter().map(|op| (seq, op.clone())));
                        }
                    }
                }
            }
            Ok(Some(Ok(_))) => {}
            _ => {}
        }
    }
    got
}

/// 在序列化树里按 id 属性找节点
fn tree_find_by_id<'a>(node: &'a Value, id_attr: &str) -> Option<&'a Value> {
    if node["a"]["id"] == id_attr {
        return Some(node);
    }
    for ch in node["c"].as_array().into_iter().flatten() {
        if let Some(h) = tree_find_by_id(ch, id_attr) {
            return Some(h);
        }
    }
    None
}

/// DOM 镜像通道（§9.2）：快照建成、输入差分（val）、节点增删差分（add）、
/// 滚动差分（scr）、seq 单调、dom-resync 后新快照收敛到最新页面状态。
const PRECHK_HTML: &str = r#"<!DOCTYPE html><html lang="zh"><head><meta charset="utf-8"><title>预检</title></head>
<body>
<button id="b1">按钮一</button> <button id="b2">按钮二</button>
<label>文本框 <input id="t" type="text"></label>
<button id="mv" style="position:relative">会移动</button>
<button id="dis">会禁用</button>
<button id="rm">会消失</button>
<iframe src="__FRAMEORIGIN__/precheck_frame.html" width="300" height="60"></iframe>
<script>
setTimeout(() => {
  document.getElementById('mv').style.left = '120px';
  document.getElementById('mv').style.top = '40px';
  document.getElementById('dis').disabled = true;
  document.getElementById('rm').remove();
}, __DELAY__);
</script>
</body></html>"#;

const PRECHK_FRAME_HTML: &str = r#"<!DOCTYPE html><html><head><meta charset="utf-8"></head>
<body style="margin:0"><button id="fb">框内按钮</button></body></html>"#;

const PRECHK_NAV_HTML: &str = r#"<!DOCTYPE html><html lang="zh"><head><meta charset="utf-8"><title>将跳转</title></head>
<body><button id="go">离开前的按钮</button>
<script>setTimeout(() => { location.href = '/precheck_after.html'; }, 1500);</script></body></html>"#;

const PRECHK_AFTER_HTML: &str = r#"<!DOCTYPE html><html lang="zh"><head><meta charset="utf-8"><title>已跳转</title></head>
<body><button id="go2">离开后的按钮</button></body></html>"#;

fn precheck_counts(port: u16) -> (u64, u64) {
    let (_, h) = http_json(port, "GET", "/health", None);
    (
        h["act_precheck"]["fast_hit"].as_u64().unwrap(),
        h["act_precheck"]["fast_miss"].as_u64().unwrap(),
    )
}

/// 动作前单元素快速复核：①页面没变时主页面元素全部走快路径（说明
/// 单元素算出的 role/name/states/bbox 与整页构建一致）；②页面自己
/// 变了（移位/禁用/消失/换文档）时必须仍被拒，快路径不得放过；
/// ③子帧内元素交给整页快照（miss），行为不变。
#[tokio::test]
async fn edge_act_fast_precheck() {
    if std::env::var("RIG_EDGE_IT").is_err() {
        eprintln!("跳过：未设置 RIG_EDGE_IT");
        return;
    }
    let _serial = SERIAL.lock().await;
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let origin = format!("http://127.0.0.1:{port}");
    // 页面变化延迟 2.5s：先让快路径验证在“没变”时全部命中
    serve(
        listener,
        vec![
            (
                "/p.html".to_string(),
                PRECHK_HTML
                    .replace("__DELAY__", "6000")
                    // 跨源（localhost ≠ 127.0.0.1）→ 独立 OOPIF 会话
                    .replace("__FRAMEORIGIN__", &format!("http://localhost:{port}")),
            ),
            (
                "/precheck_frame.html".to_string(),
                PRECHK_FRAME_HTML.to_string(),
            ),
            ("/nav.html".to_string(), PRECHK_NAV_HTML.to_string()),
            (
                "/precheck_after.html".to_string(),
                PRECHK_AFTER_HTML.to_string(),
            ),
        ],
    );
    let edge_port = free_port();
    let mut proc = spawn_edge(&[
        "--listen".into(),
        format!("127.0.0.1:{edge_port}"),
        "--url".into(),
        format!("{origin}/p.html"),
        "--allow-origin".into(),
        origin.clone(),
        "--width".into(),
        "900".into(),
        "--height".into(),
        "700".into(),
    ]);
    wait_health(edge_port).await;
    let _ = &mut proc;
    tokio::time::sleep(Duration::from_millis(500)).await;

    // ── ① 没变：逐个点主页面按钮/聚焦文本框，全部快路径命中 ──
    let mut obs = observe(edge_port);
    let mut frame = obs["observation"]["frame_id"].as_u64().unwrap();
    let click = |id: &str| json!({"kind": "click", "target": {"by": "element", "id": id}});
    let (h0, m0) = precheck_counts(edge_port);
    for name in ["按钮一", "按钮二"] {
        let id = find_element(&obs["observation"], "button", name)
            .unwrap_or_else(|| panic!("找不到按钮 {name}: {obs}"))["id"]
            .as_str()
            .unwrap()
            .to_string();
        let r = act(edge_port, frame, click(&id));
        assert_eq!(r["result"], "done", "没变时点击应成功: {r}");
        frame = r["frame_id"].as_u64().unwrap();
        obs = observe(edge_port);
        frame = obs["observation"]["frame_id"].as_u64().unwrap().max(frame);
    }
    let (h1, m1) = precheck_counts(edge_port);
    assert_eq!(
        h1 - h0,
        2,
        "主页面元素应全部走快路径（hit {h0}→{h1}, miss {m0}→{m1}）"
    );
    assert_eq!(m1, m0, "没变时不应有退回整页");

    // ── ③ 子帧内元素：快路径不适用 → miss，行为仍正确 ──
    let fb = find_element(&obs["observation"], "button", "框内按钮").expect("子帧按钮应在快照里")
        ["id"]
        .as_str()
        .unwrap()
        .to_string();
    let r = act(edge_port, frame, click(&fb));
    assert_eq!(r["result"], "done", "子帧点击应成功: {r}");
    let (h2, m2) = precheck_counts(edge_port);
    assert_eq!(h2, h1, "子帧元素不走快路径");
    assert_eq!(m2, m1 + 1, "子帧元素应记一次 miss");

    // ── ② 页面自己变了：移位/禁用/消失都必须被拒 ──
    let obs = observe(edge_port);
    let frame = obs["observation"]["frame_id"].as_u64().unwrap();
    let ids: Vec<String> = ["会移动", "会禁用", "会消失"]
        .iter()
        .map(|n| {
            find_element(&obs["observation"], "button", n).unwrap()["id"]
                .as_str()
                .unwrap()
                .to_string()
        })
        .collect();
    tokio::time::sleep(Duration::from_secs(7)).await; // 页面脚本已改动，且期间无人再 observe
    for (name, id) in ["会移动", "会禁用", "会消失"].iter().zip(&ids) {
        let r = act(edge_port, frame, click(id));
        assert_eq!(
            r["result"], "rejected",
            "页面已变（{name}），旧帧上的动作必须被拒: {r}"
        );
    }
    let (_, m3) = precheck_counts(edge_port);
    assert!(
        m3 >= m2 + 3,
        "三次都应从快路径退回整页裁决（miss {m2}→{m3}）"
    );

    // ── ② 换文档：同帧号、同元素 id，文档已换 → 被拒 ──
    let r = act(
        edge_port,
        frame,
        json!({"kind": "navigate", "url": format!("{origin}/nav.html")}),
    );
    assert_eq!(r["result"], "done", "导航应成功: {r}");
    let obs = observe(edge_port);
    let frame = obs["observation"]["frame_id"].as_u64().unwrap();
    let go = find_element(&obs["observation"], "button", "离开前的按钮")
        .expect("导航后应见到 nav 页按钮")["id"]
        .as_str()
        .unwrap()
        .to_string();
    tokio::time::sleep(Duration::from_secs(3)).await; // 页面自己跳到 after 页
    let r = act(edge_port, frame, click(&go));
    assert_eq!(r["result"], "rejected", "文档已换，旧元素必须被拒: {r}");
}

const UNCH_TXT_HTML: &str = r#"<!DOCTYPE html><html lang="zh"><head><meta charset="utf-8"><title>文字</title></head>
<body><button id="a">按钮甲</button> <p id="lbl">状态：旧</p>
<script>setTimeout(() => { document.getElementById('lbl').textContent = '状态：新'; }, 5000);</script>
</body></html>"#;

const UNCH_SCROLL_HTML: &str = r#"<!DOCTYPE html><html lang="zh"><head><meta charset="utf-8"><title>滚动</title></head>
<body><div id="box"></div>
<script>
const box = document.getElementById('box');
for (let i = 0; i < 60; i++) { const b = document.createElement('button'); b.textContent = '行' + i; b.style.display = 'block'; b.style.height = '40px'; box.appendChild(b); }
setTimeout(() => window.scrollTo(0, 800), 5000);
</script></body></html>"#;

const UNCH_CLICK_HTML: &str = r#"<!DOCTYPE html><html lang="zh"><head><meta charset="utf-8"><title>点击</title></head>
<body><button id="chg" onclick="document.getElementById('lbl').textContent='已点击'">会改文字</button>
<button id="nop">什么都不改</button><p id="lbl">未点击</p></body></html>"#;

const UNCH_GEOMETRY_HTML: &str = r#"<!DOCTYPE html><html lang="zh"><head><meta charset="utf-8"><title>几何</title>
<style>@keyframes slide { to { transform: translateX(150px); } }
#move { position:absolute;left:20px;top:40px; }
#move:focus { animation:slide 1ms forwards; }</style></head>
<body><button id="move">移动按钮</button>
</body></html>"#;

const UNCH_NESTED_SCROLL_HTML: &str = r#"<!doctype html><meta charset="utf-8">
<style>#scroller { width: 300px; height: 180px; overflow: auto; }
#padding { height: 400px; }</style>
<div id="scroller"><div id="padding"><button>嵌套滚动按钮</button></div></div>
<script>setTimeout(() => document.getElementById('scroller').scrollTop = 100, 5000);</script>"#;

fn observe_since(port: u16, since: u64) -> Value {
    let (s, v) = http_json(
        port,
        "POST",
        "/observe",
        Some(json!({"mode": "tree", "since_frame": since})),
    );
    assert_eq!(s, 200, "增量 observe 应成功: {v}");
    v
}

fn unchanged_count(port: u16) -> u64 {
    let (_, h) = http_json(port, "GET", "/health", None);
    h["changes"]["observe_unchanged"].as_u64().unwrap()
}

/// 观察采集候选帧，反映静态、文字、滚动、动画、焦点与导航状态。
#[tokio::test]
async fn edge_observe_unchanged_vs_changed() {
    if std::env::var("RIG_EDGE_IT").is_err() {
        eprintln!("跳过：未设置 RIG_EDGE_IT");
        return;
    }
    let _serial = SERIAL.lock().await;
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    serve(
        listener,
        vec![
            ("/txt.html".to_string(), UNCH_TXT_HTML.to_string()),
            ("/scroll.html".to_string(), UNCH_SCROLL_HTML.to_string()),
            (
                "/nested.html".to_string(),
                UNCH_NESTED_SCROLL_HTML.to_string(),
            ),
            ("/click.html".to_string(), UNCH_CLICK_HTML.to_string()),
            ("/geometry.html".to_string(), UNCH_GEOMETRY_HTML.to_string()),
        ],
    );
    for page in ["txt.html", "scroll.html", "nested.html"] {
        let edge_port = free_port();
        let mut proc = spawn_edge(&[
            "--listen".into(),
            format!("127.0.0.1:{edge_port}"),
            "--url".into(),
            format!("http://127.0.0.1:{port}/{page}"),
            "--width".into(),
            "900".into(),
            "--height".into(),
            "600".into(),
        ]);
        wait_health(edge_port).await;
        let _ = &mut proc;
        // 等页面脚本的定时变化/滚动发生之前先取基线帧
        let f = observe(edge_port)["observation"]["frame_id"]
            .as_u64()
            .unwrap();
        let before = unchanged_count(edge_port);

        // ① 页面还没变：采集候选帧并回空增量
        let d = observe_since(edge_port, f);
        assert!(
            d["delta"]["frame_id"].as_u64().unwrap() > f,
            "{page}: 同投影仍應以候選觀察回覆: {d}"
        );
        assert!(d["delta"]["upserts"].as_array().unwrap().is_empty());
        assert_eq!(
            unchanged_count(edge_port),
            before + 1,
            "{page}: 应记一次空增量"
        );

        // ② 页面自己变了/滚动了（无人操作）：必须重新采集并带出变化
        tokio::time::sleep(Duration::from_millis(5500)).await;
        let d = observe_since(edge_port, f);
        assert!(
            d["delta"]["frame_id"].as_u64().unwrap() > f,
            "{page}: 页面已变，必须重新采集: {d}"
        );
        assert!(
            !d["delta"]["upserts"].as_array().unwrap().is_empty()
                || !d["delta"]["removed"].as_array().unwrap().is_empty(),
            "{page}: 增量应带出变化: {d}"
        );
    }

    // 無 CDP DOM 通知的 CSS 幾何變化仍須由候選觀察捕捉。
    let edge_port = free_port();
    let mut proc = spawn_edge(&[
        "--listen".into(),
        format!("127.0.0.1:{edge_port}"),
        "--url".into(),
        format!("http://127.0.0.1:{port}/geometry.html"),
        "--width".into(),
        "900".into(),
        "--height".into(),
        "600".into(),
    ]);
    wait_health(edge_port).await;
    let _ = &mut proc;
    let deadline = Instant::now() + Duration::from_secs(5);
    let first = loop {
        let observation = observe(edge_port);
        if find_element(&observation["observation"], "button", "移动按钮").is_some() {
            break observation;
        }
        assert!(Instant::now() < deadline, "几何夹具尚未就绪");
        tokio::time::sleep(Duration::from_millis(50)).await;
    };
    let mut applied: rig_proto::observation::Observation =
        serde_json::from_value(first["observation"].clone()).unwrap();
    let frame = applied.frame_id;
    let id = find_element(&first["observation"], "button", "移动按钮").unwrap()["id"]
        .as_str()
        .unwrap();
    let before_x = applied.find(id).unwrap().bbox.unwrap().0[0];
    let click = act(
        edge_port,
        frame,
        json!({"kind":"click", "target":{"by":"element", "id":id}}),
    );
    assert_eq!(click["result"], "done");
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        let response = observe_since(edge_port, applied.frame_id);
        let delta: rig_proto::observation::ObservationDelta =
            serde_json::from_value(response["delta"].clone())
                .unwrap_or_else(|_| panic!("同文档几何变化应返回差分: {response}"));
        applied = applied.apply_delta(&delta).expect("几何差分应能应用");
        if applied.find(id).unwrap().bbox.unwrap().0[0] >= before_x + 149.0 {
            break;
        }
        assert!(Instant::now() < deadline, "差分未反映按钮位移: {response}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let full = observe(edge_port);
    let expected: rig_proto::observation::Observation =
        serde_json::from_value(full["observation"].clone()).unwrap();
    applied.frame_id = expected.frame_id;
    assert_eq!(applied, expected, "套用幾何差分後應與完整觀察一致");
    let nav = act(
        edge_port,
        full["observation"]["frame_id"].as_u64().unwrap(),
        json!({"kind":"navigate", "url":format!("http://127.0.0.1:{port}/txt.html")}),
    );
    assert_eq!(nav["result"], "done", "同分頁導航應成功: {nav}");
    tokio::time::sleep(Duration::from_millis(300)).await;
    let after_nav = observe_since(edge_port, frame);
    assert!(
        after_nav.get("observation").is_some() && after_nav.get("delta").is_none(),
        "跨文件 since_frame 必須回完整觀察: {after_nav}"
    );

    // ── 操作与呈现解耦：act 做完即返回，页面因动作变成什么样按需取增量 ──
    let edge_port = free_port();
    let mut proc = spawn_edge(&[
        "--listen".into(),
        format!("127.0.0.1:{edge_port}"),
        "--url".into(),
        format!("http://127.0.0.1:{port}/click.html"),
        "--width".into(),
        "900".into(),
        "--height".into(),
        "600".into(),
    ]);
    wait_health(edge_port).await;
    let _ = &mut proc;
    let obs = observe(edge_port);
    let f = obs["observation"]["frame_id"].as_u64().unwrap();
    let id_of = |name: &str| {
        find_element(&obs["observation"], "button", name).unwrap()["id"]
            .as_str()
            .unwrap()
            .to_string()
    };
    let click = |id: &str| json!({"kind": "click", "target": {"by": "element", "id": id}});
    // 點擊不改文字，但會改變焦點；since_frame 必須回報 Focused 狀態。
    let r = act(edge_port, f, click(&id_of("什么都不改")));
    assert_eq!(r["result"], "done", "{r}");
    assert_eq!(r["frame_id"], f, "Done 应指向动作所基于的校验帧: {r}");
    let d = observe_since(edge_port, f);
    assert!(
        d["delta"]["frame_id"].as_u64().unwrap() > f,
        "焦點變化應推進候選幀: {d}"
    );
    assert!(
        d["delta"]["upserts"].to_string().contains("focused"),
        "差分應反映 focused: {d}"
    );
    let focused_full = observe(edge_port);
    let focused_base: rig_proto::observation::Observation =
        serde_json::from_value(obs["observation"].clone()).unwrap();
    let focused_delta: rig_proto::observation::ObservationDelta =
        serde_json::from_value(d["delta"].clone()).unwrap();
    let mut focused_applied = focused_base
        .apply_delta(&focused_delta)
        .expect("焦點差分應可套用");
    let focused_expected: rig_proto::observation::Observation =
        serde_json::from_value(focused_full["observation"].clone()).unwrap();
    focused_applied.frame_id = focused_expected.frame_id;
    assert_eq!(
        focused_applied, focused_expected,
        "套用焦點差分後應與完整觀察一致"
    );
    let f = focused_full["observation"]["frame_id"].as_u64().unwrap();
    // 点“会改文字”：随后取增量必须带出文字变化
    let r = act(edge_port, f, click(&id_of("会改文字")));
    assert_eq!(r["result"], "done", "{r}");
    tokio::time::sleep(Duration::from_millis(300)).await;
    let d = observe_since(edge_port, f);
    assert!(
        d["delta"]["frame_id"].as_u64().unwrap() > f,
        "页面变了，应重新采集: {d}"
    );
    assert!(
        d["text"].as_str().unwrap_or("").contains("增量")
            && d["delta"]["upserts"].to_string().contains("已点击"),
        "增量应带出新文字: {d}"
    );
}

/// 人通道開啟只允許觀看；沒有實際消費者時不應持續採集整頁畫面。
#[tokio::test]
async fn edge_human_zero_viewers_stops_full_frame_pump() {
    if std::env::var("RIG_EDGE_IT").is_err() {
        eprintln!("跳过：未设置 RIG_EDGE_IT");
        return;
    }
    let _serial = SERIAL.lock().await;
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let anim = r#"<!DOCTYPE html><html><body style="margin:0"><div style="width:120px;height:120px;background:red;animation:turn 300ms linear infinite alternate"></div>
<style>@keyframes turn {to {transform:translateX(180px)}}</style></body></html>"#;
    let static_page = r#"<!doctype html><title>静态 B</title>
<body style="background:#22cc44"><p>静态 B</p></body>"#;
    serve(
        listener,
        vec![
            ("/idle.html".into(), anim.into()),
            ("/static.html".into(), static_page.into()),
        ],
    );
    let edge_port = free_port();
    let _proc = spawn_edge(&[
        "--listen".into(),
        format!("127.0.0.1:{edge_port}"),
        "--url".into(),
        format!("http://127.0.0.1:{port}/idle.html"),
        "--human".into(),
        "--width".into(),
        "900".into(),
        "--height".into(),
        "600".into(),
    ]);
    wait_health(edge_port).await;
    tokio::time::sleep(Duration::from_millis(700)).await;
    let first = http_json(edge_port, "GET", "/health", None).1;
    assert_eq!(
        first["human_running"], true,
        "--human 應開啟可接入通道: {first}"
    );
    assert_eq!(
        first["media"]["running"], false,
        "未要求 AV 時不應啟動 HLS: {first}"
    );
    let counters = |health: &Value| {
        [
            health["pump"]["full"].as_u64().unwrap(),
            health["latency"]["compose"]["count"].as_u64().unwrap(),
            health["latency"]["encode"]["count"].as_u64().unwrap(),
        ]
    };
    let baseline = counters(&first);
    tokio::time::sleep(Duration::from_secs(2)).await;
    let later = http_json(edge_port, "GET", "/health", None).1;
    assert_eq!(
        counters(&later),
        baseline,
        "零觀看者時整頁採集、解碼與編碼計數應停止增加: {first} → {later}"
    );

    use tokio_tungstenite::{tungstenite::Message, WebSocketStream};
    async fn frame_within(
        ws: &mut WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>,
        what: &str,
    ) -> Vec<u8> {
        let deadline = Instant::now() + Duration::from_secs(6);
        loop {
            assert!(Instant::now() < deadline, "{what}: 等不到整頁影格");
            match tokio::time::timeout(Duration::from_millis(500), ws.next()).await {
                Ok(Some(Ok(Message::Binary(frame)))) => return frame.to_vec(),
                Ok(Some(Ok(_))) | Err(_) => continue,
                other => panic!("{what}: WS 結束或失敗: {other:?}"),
            }
        }
    }
    async fn wait_full_stable(edge_port: u16) {
        let deadline = Instant::now() + Duration::from_secs(8);
        let mut stable = 0;
        let mut previous = http_json(edge_port, "GET", "/health", None).1["pump"]["full"]
            .as_u64()
            .unwrap();
        while stable < 4 {
            assert!(
                Instant::now() < deadline,
                "最後一位觀看者離開後影格泵未停止"
            );
            tokio::time::sleep(Duration::from_millis(300)).await;
            let now = http_json(edge_port, "GET", "/health", None).1["pump"]["full"]
                .as_u64()
                .unwrap();
            if now == previous {
                stable += 1;
            } else {
                stable = 0;
                previous = now;
            }
        }
    }
    // 啟動中的連線立刻斷開，不得留下消費者需求。
    let (early, _) = tokio_tungstenite::connect_async(format!("ws://127.0.0.1:{edge_port}/ws"))
        .await
        .expect("早退觀看連線應可建立");
    drop(early);
    wait_full_stable(edge_port).await;
    let (mut a, _) = tokio_tungstenite::connect_async(format!("ws://127.0.0.1:{edge_port}/ws"))
        .await
        .unwrap();
    let (mut b, _) = tokio_tungstenite::connect_async(format!("ws://127.0.0.1:{edge_port}/ws"))
        .await
        .unwrap();
    frame_within(&mut a, "A 接入").await;
    frame_within(&mut b, "B 接入").await;
    drop(a);
    frame_within(&mut b, "A 離開後 B 仍接收").await;
    let (mut c, _) = tokio_tungstenite::connect_async(format!("ws://127.0.0.1:{edge_port}/ws"))
        .await
        .unwrap();
    frame_within(&mut c, "C 接入").await;
    drop(b);
    frame_within(&mut c, "B 離開後 C 仍接收").await;
    use futures_util::SinkExt;
    for message in [
        json!({"type":"jpeg-demand","on":false}),
        json!({"type":"chrome","action":"new-tab"}),
        json!({"type":"chrome","action":"navigate","url":format!("http://127.0.0.1:{port}/static.html")}),
    ] {
        c.send(Message::Text(message.to_string().into()))
            .await
            .unwrap();
    }
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let observation = observe(edge_port);
        if observation["text"]
            .as_str()
            .unwrap_or_default()
            .contains("静态 B")
        {
            break;
        }
        assert!(Instant::now() < deadline, "无画面需求时应能切到静态标签页");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    drop(c);
    wait_full_stable(edge_port).await;
    let (mut reconnect, _) =
        tokio_tungstenite::connect_async(format!("ws://127.0.0.1:{edge_port}/ws"))
            .await
            .unwrap();
    let frame = frame_within(&mut reconnect, "静态页首帧").await;
    let mut decoder = Command::new("ffmpeg")
        .args([
            "-v",
            "error",
            "-f",
            "mjpeg",
            "-i",
            "pipe:0",
            "-vf",
            "crop=2:2:450:300,format=rgb24",
            "-frames:v",
            "1",
            "-f",
            "rawvideo",
            "pipe:1",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("用 FFmpeg 解码 Chromium 首帧");
    decoder
        .stdin
        .take()
        .unwrap()
        .write_all(&frame[8..])
        .unwrap();
    let pixel = decoder.wait_with_output().unwrap();
    assert!(pixel.status.success() && pixel.stdout.len() >= 3);
    assert!(
        pixel.stdout[0] < 65 && pixel.stdout[1] > 170,
        "重连首帧应来自绿色静态 B: {:?}",
        &pixel.stdout[..3]
    );
    drop(reconnect);
    wait_full_stable(edge_port).await;
}

/// Viewer 摘要不啟動完整 DOM 鏡像；視口回覆及 RTC 音視需求各自記帳。
#[tokio::test(flavor = "multi_thread")]
async fn edge_viewer_summary_viewport_and_rtc_audio_demand() {
    if std::env::var("RIG_EDGE_IT").is_err() {
        eprintln!("跳过：未设置 RIG_EDGE_IT");
        return;
    }
    let _serial = SERIAL.lock().await;
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let page_port = listener.local_addr().unwrap().port();
    let page = r#"<!doctype html><html><body style="margin:0"><video style="width:240px;height:120px;background:#123" aria-label="本地视频摘要"></video>
<canvas width="200" height="100" style="background:#456"></canvas><button>摘要按钮</button>
<div style="width:80px;height:50px;background:#789"></div></body></html>"#;
    serve(listener, vec![("/summary.html".into(), page.into())]);
    let edge_port = free_port();
    let _proc = spawn_edge(&[
        "--listen".into(),
        format!("127.0.0.1:{edge_port}"),
        "--url".into(),
        format!("http://127.0.0.1:{page_port}/summary.html"),
        "--human".into(),
        "--av".into(),
        "--width".into(),
        "900".into(),
        "--height".into(),
        "600".into(),
    ]);
    wait_health(edge_port).await;

    use futures_util::SinkExt;
    use tokio_tungstenite::tungstenite::Message;
    let (mut ws, _) = tokio_tungstenite::connect_async(format!("ws://127.0.0.1:{edge_port}/ws"))
        .await
        .unwrap();
    ws.send(Message::Text(
        json!({"type":"mode-summary", "request":1})
            .to_string()
            .into(),
    ))
    .await
    .unwrap();
    let (summary, saw_initial_viewport) = tokio::time::timeout(Duration::from_secs(8), async {
        let mut saw_viewport = false;
        loop {
            match ws.next().await {
                Some(Ok(Message::Text(text))) => {
                    let v: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
                    if v["type"] == "hello" {
                        assert_eq!(v["cssWidth"].as_f64(), Some(900.0), "初始視口寬度: {v}");
                        assert_eq!(v["cssHeight"].as_f64(), Some(600.0), "初始視口高度: {v}");
                        saw_viewport = true;
                    }
                    if v["type"] == "mode-summary" && v["request"] == 1 {
                        break (v, saw_viewport);
                    }
                }
                Some(Ok(_)) => {}
                other => panic!("mode-summary WS 結束: {other:?}"),
            }
        }
    })
    .await
    .expect("mode-summary 回覆超時");
    assert!(saw_initial_viewport, "應收到初始 900x600 視口值");
    for key in ["videoRatio", "canvasRatio", "regionRatio", "interactive"] {
        assert!(summary[key].is_number(), "摘要缺少數值 {key}: {summary}");
    }
    assert!(
        summary["interactive"].as_f64().unwrap() > 0.0,
        "本機按鈕應計入互動摘要: {summary}"
    );
    let health = http_json(edge_port, "GET", "/health", None).1;
    assert_eq!(health["dom_viewers"], 0, "摘要不得訂閱完整 DOM: {health}");
    let drain_until = Instant::now() + Duration::from_millis(500);
    while Instant::now() < drain_until {
        if let Ok(Some(Ok(Message::Text(text)))) =
            tokio::time::timeout(Duration::from_millis(100), ws.next()).await
        {
            let v: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
            assert_ne!(v["type"], "dom-snapshot", "摘要前不應收到 DOM 快照: {v}");
        }
    }
    ws.send(Message::Text(
        json!({"type":"dom-start"}).to_string().into(),
    ))
    .await
    .unwrap();
    let _snapshot = next_dom(&mut ws, "dom-snapshot", Duration::from_secs(12)).await;
    ws.send(Message::Text(json!({"type":"dom-stop"}).to_string().into()))
        .await
        .unwrap();
    let dom_deadline = Instant::now() + Duration::from_secs(5);
    while http_json(edge_port, "GET", "/health", None).1["dom_viewers"] != 0 {
        assert!(Instant::now() < dom_deadline, "dom-stop 應釋放快照訂閱");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    // 視口尺寸由 Viewer 明確要求，邊緣回覆實際套用值與同一 request id。
    ws.send(Message::Text(
        json!({"type":"resize", "request":2, "w":800, "h":500})
            .to_string()
            .into(),
    ))
    .await
    .unwrap();
    let resized = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Some(Ok(Message::Text(text))) = ws.next().await {
                let v: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
                if v["type"] == "viewport" && v["request"] == 2 {
                    break v;
                }
            }
        }
    })
    .await
    .expect("viewport 套用回覆超時");
    assert_eq!(
        resized["cssWidth"], 800,
        "viewport 寬度應回實際套用值: {resized}"
    );
    assert_eq!(
        resized["cssHeight"], 500,
        "viewport 高度應回實際套用值: {resized}"
    );

    // RTC 僅要音訊時不應索取視頻；jpeg-demand 關閉後畫面採集應停止。
    ws.send(Message::Text(
        json!({"type":"jpeg-demand", "on":false}).to_string().into(),
    ))
    .await
    .unwrap();
    ws.send(Message::Text(
        json!({"type":"rtc-start", "video":false, "audio":true})
            .to_string()
            .into(),
    ))
    .await
    .unwrap();
    let deadline = Instant::now() + Duration::from_secs(8);
    loop {
        let h = http_json(edge_port, "GET", "/health", None).1;
        if h["rtc_video_want"] == 0 && h["rtc_audio_want"] == 1 {
            break;
        }
        assert!(Instant::now() < deadline, "音訊單獨需求記帳錯誤: {h}");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let pump_after_audio = http_json(edge_port, "GET", "/health", None).1["pump"]["full"]
        .as_u64()
        .unwrap();
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert_eq!(
        http_json(edge_port, "GET", "/health", None).1["pump"]["full"]
            .as_u64()
            .unwrap(),
        pump_after_audio,
        "純音訊 RTC 不應保留整頁視頻採集"
    );

    ws.send(Message::Text(
        json!({"type":"rtc-demand", "video":true, "audio":true})
            .to_string()
            .into(),
    ))
    .await
    .unwrap();
    let deadline = Instant::now() + Duration::from_secs(8);
    loop {
        let h = http_json(edge_port, "GET", "/health", None).1;
        if h["rtc_video_want"] == 1 && h["rtc_audio_want"] == 1 {
            break;
        }
        assert!(Instant::now() < deadline, "RTC 視頻需求未生效: {h}");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    ws.send(Message::Text(
        json!({"type":"rtc-demand", "video":false, "audio":true})
            .to_string()
            .into(),
    ))
    .await
    .unwrap();
    let deadline = Instant::now() + Duration::from_secs(8);
    loop {
        let h = http_json(edge_port, "GET", "/health", None).1;
        if h["rtc_video_want"] == 0 && h["rtc_audio_want"] == 1 {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "關閉 RTC 視頻不得清除音訊需求: {h}"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    ws.send(Message::Text(json!({"type":"rtc-stop"}).to_string().into()))
        .await
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(8);
    loop {
        let h = http_json(edge_port, "GET", "/health", None).1;
        if h["rtc_video_want"] == 0 && h["rtc_audio_want"] == 0 {
            break;
        }
        assert!(Instant::now() < deadline, "停止 RTC 應釋放音視頻需求: {h}");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// 回归（审查 P1）：后台 DOM 感知订阅不得停发 WS JPEG 帧——
/// jpeg-demand 才是帧流需求的唯一来源。
#[tokio::test(flavor = "multi_thread")]
async fn edge_dom_subscription_keeps_jpeg_frames() {
    if std::env::var("RIG_EDGE_IT").is_err() {
        eprintln!("跳过：未设置 RIG_EDGE_IT");
        return;
    }
    let _serial = SERIAL.lock().await;
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let anim = "<html><body><div id=b style='width:80px;height:80px;background:red'></div>\
        <script>let n=0;setInterval(()=>{document.getElementById('b').style.marginLeft=(n++%200)+'px'},30)</script>\
        </body></html>";
    serve(listener, vec![("/anim.html".to_string(), anim.to_string())]);
    let edge_port = free_port();
    let mut proc = spawn_edge(&[
        "--listen".into(),
        format!("127.0.0.1:{edge_port}"),
        "--url".into(),
        format!("http://127.0.0.1:{port}/anim.html"),
        "--human".into(),
    ]);
    wait_health(edge_port).await;
    let _ = &mut proc;

    use futures_util::SinkExt;
    let (mut ws, _) = tokio_tungstenite::connect_async(format!("ws://127.0.0.1:{edge_port}/ws"))
        .await
        .expect("人通道 WS 应可连接");
    ws.send(tokio_tungstenite::tungstenite::Message::Text(
        json!({"type": "dom-start"}).to_string().into(),
    ))
    .await
    .unwrap();
    // 订阅 DOM 后 2 秒内仍应持续收到二进制帧（至少 3 张）
    let mut frames = 0;
    let end = Instant::now() + Duration::from_secs(2);
    while Instant::now() < end {
        if let Ok(Some(Ok(tokio_tungstenite::tungstenite::Message::Binary(_)))) =
            tokio::time::timeout(Duration::from_millis(500), ws.next()).await
        {
            frames += 1;
        }
    }
    assert!(frames >= 3, "后台 DOM 订阅下应仍收 JPEG 帧，实际 {frames}");
    // 观看端声明不消费 JPEG（前台 DOM/RTC）后应停发
    ws.send(tokio_tungstenite::tungstenite::Message::Text(
        json!({"type": "jpeg-demand", "on": false})
            .to_string()
            .into(),
    ))
    .await
    .unwrap();
    // 排空在途消息（页面在动，DOM 差分文本会持续到达，按时限排空）
    let drain_end = Instant::now() + Duration::from_millis(800);
    while Instant::now() < drain_end {
        let _ = tokio::time::timeout(Duration::from_millis(100), ws.next()).await;
    }
    let mut late = 0;
    let end = Instant::now() + Duration::from_secs(1);
    while Instant::now() < end {
        if let Ok(Some(Ok(tokio_tungstenite::tungstenite::Message::Binary(_)))) =
            tokio::time::timeout(Duration::from_millis(300), ws.next()).await
        {
            late += 1;
        }
    }
    assert_eq!(late, 0, "jpeg-demand off 后不应再收帧");
}

#[tokio::test]
async fn edge_dom_channel() {
    if std::env::var("RIG_EDGE_IT").is_err() {
        eprintln!("跳过：未设置 RIG_EDGE_IT");
        return;
    }
    let _serial = SERIAL.lock().await;
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let origin = format!("http://127.0.0.1:{port}");
    serve(
        listener,
        vec![
            ("/dom.html".to_string(), DOM_HTML.to_string()),
            ("/frame.html".to_string(), FRAME_HTML.to_string()),
        ],
    );
    let edge_port = free_port();
    let mut proc = spawn_edge(&[
        "--listen".into(),
        format!("127.0.0.1:{edge_port}"),
        "--url".into(),
        format!("{origin}/dom.html"),
        "--human".into(),
        "--width".into(),
        "900".into(),
        "--height".into(),
        "700".into(),
    ]);
    wait_health(edge_port).await;
    let _ = &mut proc;
    tokio::time::sleep(Duration::from_millis(800)).await;

    use futures_util::SinkExt;
    let (mut ws, _) = tokio_tungstenite::connect_async(format!("ws://127.0.0.1:{edge_port}/ws"))
        .await
        .expect("人通道 WS 应可连接");
    macro_rules! ws_send {
        ($v:expr) => {
            ws.send(tokio_tungstenite::tungstenite::Message::Text(
                $v.to_string().into(),
            ))
            .await
            .unwrap()
        };
    }

    // ── 1. 快照建成 ──
    ws_send!(json!({"type": "dom-start"}));
    let snap = next_dom(&mut ws, "dom-snapshot", Duration::from_secs(20)).await;
    let seq0 = snap["seq"].as_u64().expect("快照应带 seq");
    assert_eq!(snap["w"], 900, "快照宽应为视口宽: {snap}");
    assert!(
        tree_find_by_id(&snap["tree"], "name").is_some(),
        "快照树应含 #name 输入框"
    );
    assert!(
        snap["css"].as_str().unwrap_or("").contains("#name"),
        "快照应带页面 CSS"
    );

    // ── 2. 镜像输入 → 源页面打字 → val 差分回流 ──
    ws_send!(json!({"type": "mouse", "dom": true, "action": "down", "x": 110, "y": 25}));
    ws_send!(json!({"type": "mouse", "dom": true, "action": "up", "x": 110, "y": 25}));
    tokio::time::sleep(Duration::from_millis(200)).await;
    ws_send!(json!({"type": "text", "dom": true, "text": "hello-dom"}));
    let got = collect_deltas_until(
        &mut ws,
        |ops| {
            ops.iter()
                .any(|(_, op)| op["o"] == "val" && op["v"] == "hello-dom")
        },
        Duration::from_secs(15),
    )
    .await;
    assert!(
        got.iter()
            .any(|(_, op)| op["o"] == "val" && op["v"] == "hello-dom"),
        "打字应产生 val 差分，实际收到: {got:?}"
    );
    // seq 在差分流内严格单调（快照可穿插占号，故不断言 +1）
    let seqs: Vec<u64> = got.iter().map(|(s, _)| *s).collect();
    assert!(
        seqs.windows(2).all(|w| w[1] > w[0]),
        "差分 seq 应严格递增: {seqs:?}"
    );
    assert!(seqs.iter().all(|s| *s > seq0), "差分 seq 应在快照之后");

    // ── 3. 点击按钮 → 节点新增差分 ──
    ws_send!(json!({"type": "mouse", "dom": true, "action": "down", "x": 70, "y": 75}));
    ws_send!(json!({"type": "mouse", "dom": true, "action": "up", "x": 70, "y": 75}));
    let got = collect_deltas_until(
        &mut ws,
        |ops| {
            ops.iter()
                .any(|(_, op)| op["o"] == "add" && op["n"].to_string().contains("added"))
        },
        Duration::from_secs(15),
    )
    .await;
    assert!(
        got.iter()
            .any(|(_, op)| op["o"] == "add" && op["n"].to_string().contains("added")),
        "新增节点应产生 add 差分，实际收到: {got:?}"
    );

    // ── 4. 滚轮 → 滚动差分 ──
    ws_send!(json!({"type": "wheel", "dom": true, "x": 450, "y": 350, "dx": 0, "dy": 600}));
    let got = collect_deltas_until(
        &mut ws,
        |ops| {
            ops.iter().any(|(_, op)| {
                op["o"] == "scr" && op["i"] == 0 && op["y"].as_i64().unwrap_or(0) >= 500
            })
        },
        Duration::from_secs(15),
    )
    .await;
    assert!(
        got.iter()
            .any(|(_, op)| op["o"] == "scr" && op["y"].as_i64().unwrap_or(0) >= 500),
        "滚动应产生 scr 差分（y≥500），实际收到: {got:?}"
    );

    // ── 5. dom-resync → 新快照收敛到最新状态（输入值已在树里） ──
    ws_send!(json!({"type": "dom-resync"}));
    let snap2 = next_dom(&mut ws, "dom-snapshot", Duration::from_secs(15)).await;
    assert!(
        snap2["seq"].as_u64().unwrap() > seq0,
        "重同步快照 seq 应更新"
    );
    let name_node = tree_find_by_id(&snap2["tree"], "name").expect("新快照应含 #name");
    assert_eq!(
        name_node["v"], "hello-dom",
        "重同步快照应收敛到已输入的值: {name_node}"
    );

    // ── 6. DOM 模式下 AI 动作 → 幽灵指针经 WS 下发（页面坐标） ──
    // 先滚回顶部：第 4 步把页面滚到 y≥500，按钮已离屏（不可交互，
    // act 会被陈旧校验正当拒绝），滚回后才点得到
    ws_send!(json!({"type": "wheel", "dom": true, "x": 450, "y": 350, "dx": 0, "dy": -3000}));
    let back = collect_deltas_until(
        &mut ws,
        |ops| {
            ops.iter()
                .any(|(_, op)| op["o"] == "scr" && op["i"] == 0 && op["y"].as_i64() == Some(0))
        },
        Duration::from_secs(10),
    )
    .await;
    assert!(
        back.iter()
            .any(|(_, op)| op["o"] == "scr" && op["y"].as_i64() == Some(0)),
        "应滚回顶部（scr y=0），实际: {back:?}"
    );
    let o = observe(edge_port);
    let frame = o["observation"]["frame_id"].as_u64().unwrap();
    let add_btn = find_element(&o["observation"], "button", "添加")
        .expect("应有添加按钮")
        .clone();
    let add_id = add_btn["id"].as_str().unwrap().to_string();
    let bb = add_btn["bbox"].as_array().expect("按钮应有 bbox").clone();
    let (cx, cy) = (
        bb[0].as_f64().unwrap() + bb[2].as_f64().unwrap() / 2.0,
        bb[1].as_f64().unwrap() + bb[3].as_f64().unwrap() / 2.0,
    );
    let r = act(
        edge_port,
        frame,
        json!({"kind": "click", "target": {"by": "element", "id": add_id}}),
    );
    assert_eq!(r["result"], "done", "点击添加应成功: {r}");
    let g = next_dom(&mut ws, "ghost", Duration::from_secs(8)).await;
    let (gx, gy) = (
        g["x"].as_f64().expect("ghost 应带 x"),
        g["y"].as_f64().expect("ghost 应带 y"),
    );
    assert!(
        (gx - cx).abs() < 40.0 && (gy - cy).abs() < 40.0,
        "ghost 应落在按钮中心附近: ghost=({gx},{gy}) center=({cx},{cy})"
    );
    assert!(
        g["label"].as_str().unwrap_or("").contains("添加"),
        "ghost 标签应含意图: {g}"
    );

    // ── 7. canvas 子流：源页 canvas 帧经 dom-canvas 到达 ──
    // 子流只发变化帧：先触发一次重同步（快照会清页内发送缓存，
    // 下一拍必重发），再等帧到达
    let cv_id = tree_find_canvas(&snap2["tree"]).expect("快照应含 CANVAS 节点");
    ws_send!(json!({"type": "dom-resync"}));
    let _snap3 = next_dom(&mut ws, "dom-snapshot", Duration::from_secs(15)).await;
    let cm = next_dom(&mut ws, "dom-canvas", Duration::from_secs(12)).await;
    assert_eq!(
        cm["i"].as_u64(),
        Some(cv_id),
        "canvas 帧应对应快照节点 id: {cm}"
    );
    let d = cm["d"].as_str().unwrap_or("");
    assert!(
        d.starts_with("data:image/png;base64,iVBOR") && d.len() > 100,
        "canvas 帧应为 PNG data URL，实际长度 {}",
        d.len()
    );

    // ── 8. 同源 iframe：内容文档经 f 子树进快照；iframe 内突变
    // 触发重同步、新快照带上新节点 ──
    ws_send!(json!({"type": "dom-resync"}));
    let snap4 = next_dom(&mut ws, "dom-snapshot", Duration::from_secs(15)).await;
    let iframe = tree_find_iframe(&snap4["tree"]).expect("快照应含 IFRAME 节点");
    let f = iframe.get("f").expect("同源 iframe 应带内容子树 f");
    assert!(
        tree_has_attr_id(f, "fin"),
        "iframe 内容应含 #fin 输入框: {f}"
    );
    // frame.html 在自身加载 900ms 后往 body 加 #late：watchFrame 的
    // 观察器应触发边缘重发快照（可能与本步的 resync 快照交错，
    // 逐个检查直到带 #late 的快照到达）
    let mut has_late = tree_has_attr_id(f, "late");
    for _ in 0..4 {
        if has_late {
            break;
        }
        let s = next_dom(&mut ws, "dom-snapshot", Duration::from_secs(15)).await;
        if let Some(fr) = tree_find_iframe(&s["tree"]) {
            if let Some(ff) = fr.get("f") {
                has_late = tree_has_attr_id(ff, "late");
            }
        }
    }
    assert!(has_late, "iframe 内突变后应收到带 #late 的新快照");

    ws_send!(json!({"type": "dom-stop"}));
    drop(ws);
}

const UPLOAD_HTML: &str = r#"<!DOCTYPE html><html lang="zh"><head><meta charset="utf-8"><title>上传</title></head>
<body>
<label>附件 <input id="up" type="file" aria-label="上传文件"></label>
<label>备注 <input id="plain" type="text" aria-label="普通文本"></label>
<p id="got">未选择</p>
<script>
document.getElementById('up').addEventListener('change', (e) => {
  const f = e.target.files[0];
  document.getElementById('got').textContent = f ? ('已收到:' + f.name + ':' + f.size) : '空';
});
</script>
</body></html>"#;

/// 按无障碍名找元素（不限角色）：文件框在 AX 里的角色随实现而异，
/// 测试只按 aria-label 给定的名字定位。
fn find_by_name<'a>(obs: &'a Value, name: &str) -> Option<&'a Value> {
    fn walk<'a>(e: &'a Value, name: &str) -> Option<&'a Value> {
        if e.get("name").and_then(Value::as_str) == Some(name) {
            return Some(e);
        }
        for c in e
            .get("children")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            if let Some(f) = walk(c, name) {
                return Some(f);
            }
        }
        None
    }
    for e in obs
        .get("elements")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        if let Some(f) = walk(e, name) {
            return Some(f);
        }
    }
    None
}

/// Upload 动作（§10.1）：给文件框喂边缘主机上的文件，页面经
/// change 事件收到；负例（非文件框、文件不存在）按码拒绝；
/// 单选文件框多文件截断并在 Done.note 注明。
#[tokio::test]
async fn edge_upload_action() {
    if std::env::var("RIG_EDGE_IT").is_err() {
        eprintln!("跳过：未设置 RIG_EDGE_IT");
        return;
    }
    let _serial = SERIAL.lock().await;
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let origin = format!("http://127.0.0.1:{port}");
    let bound = serve(listener, vec![("/upload.html".into(), UPLOAD_HTML.into())]);
    assert_eq!(bound, port);
    let edge_port = free_port();
    let _edge = spawn_edge(&[
        "--listen".into(),
        format!("127.0.0.1:{edge_port}"),
        "--url".into(),
        format!("{origin}/upload.html"),
        "--width".into(),
        "900".into(),
        "--height".into(),
        "700".into(),
    ]);
    wait_health(edge_port).await;

    // 边缘主机上的待上传文件（测试进程与边缘同机）
    let f1 = format!("/tmp/rig-upload-it-{edge_port}-a.txt");
    let f2 = format!("/tmp/rig-upload-it-{edge_port}-b.txt");
    std::fs::write(&f1, b"hello-upload").unwrap();
    std::fs::write(&f2, b"second").unwrap();

    let deadline = Instant::now() + Duration::from_secs(5);
    let obs = loop {
        let candidate = observe(edge_port);
        if find_by_name(&candidate["observation"], "上传文件").is_some()
            && find_by_name(&candidate["observation"], "普通文本").is_some()
        {
            break candidate;
        }
        assert!(
            Instant::now() < deadline,
            "观察应含文件框及文本框: {candidate}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    };
    let frame = obs["observation"]["frame_id"].as_u64().unwrap();
    let up = find_by_name(&obs["observation"], "上传文件").expect("观察应含文件框");
    let up_id = up["id"].as_str().unwrap().to_string();
    let plain = find_by_name(&obs["observation"], "普通文本").expect("观察应含文本框");
    let plain_id = plain["id"].as_str().unwrap().to_string();

    // 负例 1：不存在的路径 → file_not_found
    let r = act(
        edge_port,
        frame,
        json!({"kind": "upload", "target": {"by": "element", "id": up_id},
               "files": ["/tmp/rig-upload-it-no-such-file.bin"]}),
    );
    assert_eq!(r["result"], "rejected", "不存在文件应被拒: {r}");
    assert_eq!(r["reason"]["code"], "file_not_found", "拒绝码: {r}");

    // 负例 2：目标不是文件框 → not_file_input
    let obs = observe(edge_port);
    let frame = obs["observation"]["frame_id"].as_u64().unwrap();
    let r = act(
        edge_port,
        frame,
        json!({"kind": "upload", "target": {"by": "element", "id": plain_id},
               "files": [f1]}),
    );
    assert_eq!(r["result"], "rejected", "非文件框应被拒: {r}");
    assert_eq!(r["reason"]["code"], "not_file_input", "拒绝码: {r}");

    // 正例：单文件上传，页面 change 收到（名:大小）
    let obs = observe(edge_port);
    let frame = obs["observation"]["frame_id"].as_u64().unwrap();
    let r = act(
        edge_port,
        frame,
        json!({"kind": "upload", "target": {"by": "element", "id": up_id},
               "files": [f1]}),
    );
    assert_eq!(r["result"], "done", "上传应成功: {r}");
    let obs = observe(edge_port);
    let base = std::path::Path::new(&f1)
        .file_name()
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();
    assert!(
        obs["text"]
            .as_str()
            .unwrap_or("")
            .contains(&format!("已收到:{base}:12")),
        "页面应收到文件 {base}:12，观察文本: {}",
        obs["text"]
    );

    // 单选文件框给两个文件：只取第一个，note 注明截断
    let frame = obs["observation"]["frame_id"].as_u64().unwrap();
    let r = act(
        edge_port,
        frame,
        json!({"kind": "upload", "target": {"by": "element", "id": up_id},
               "files": [f2, f1]}),
    );
    assert_eq!(r["result"], "done", "多文件上传应成功（截断）: {r}");
    assert!(
        r["note"].as_str().unwrap_or("").contains("只取第一个"),
        "Done.note 应注明截断: {r}"
    );
    let obs = observe(edge_port);
    let base2 = std::path::Path::new(&f2)
        .file_name()
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();
    assert!(
        obs["text"]
            .as_str()
            .unwrap_or("")
            .contains(&format!("已收到:{base2}:6")),
        "页面应收到第一个文件 {base2}:6，观察文本: {}",
        obs["text"]
    );

    let _ = std::fs::remove_file(&f1);
    let _ = std::fs::remove_file(&f2);
}

fn count_json_elements(obs: &Value) -> usize {
    fn walk(e: &Value) -> usize {
        1 + e
            .get("children")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .map(walk)
            .sum::<usize>()
    }
    obs.get("elements")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .map(walk)
        .sum()
}

const MODES_HTML: &str = r#"<!DOCTYPE html><html lang="zh"><head><meta charset="utf-8"><title>模式</title></head>
<body>
<main id="wrap" aria-label="内容区">
<p>第 1 段：一些填充文字用来撑大快照体积。</p>
<p>第 2 段：一些填充文字用来撑大快照体积。</p>
<p>第 3 段：一些填充文字用来撑大快照体积。</p>
<p>第 4 段：一些填充文字用来撑大快照体积。</p>
<p>第 5 段：一些填充文字用来撑大快照体积。</p>
<p>第 6 段：一些填充文字用来撑大快照体积。</p>
<p>第 7 段：一些填充文字用来撑大快照体积。</p>
<p>第 8 段：一些填充文字用来撑大快照体积。</p>
<p>第 9 段：一些填充文字用来撑大快照体积。</p>
<p>第 10 段：一些填充文字用来撑大快照体积。</p>
<p>第 11 段：一些填充文字用来撑大快照体积。</p>
<p>第 12 段：一些填充文字用来撑大快照体积。</p>
</main>
<button id="btn">计数按钮</button>
<p id="cnt">计数:0</p>
<script>
let n = 0;
document.getElementById('btn').addEventListener('click', () => {
  n += 1;
  document.getElementById('cnt').textContent = '计数:' + n;
});
</script>
<div style="height:1500px"></div>
<button id="bottomBtn">底部按钮</button>
</body></html>"#;

const SCREENSHOT_MASK_HTML: &str = r#"<!DOCTYPE html><html lang="zh"><head><meta charset="utf-8"><title>遮罩合成</title>
<style>body{margin:0;min-height:120vh} main{padding:16px}
#secret-wrap{margin:24px;background:#21e659;padding:24px}
input[type=password]{display:block;width:180px;height:48px;padding:0;border:0;background:#f2d928;color:#111}
#ax-hidden{position:absolute;left:360px;top:190px}</style></head>
<body><main id="public" aria-label="公開區"><button>公開按鈕</button></main>
<div id="secret-wrap"><input type="password" aria-label="合成密碼" value="SYNTHETIC-ONLY"></div>
<input id="ax-hidden" type="password" aria-hidden="true" value="SYNTHETIC-ONLY"></body></html>"#;

/// R-18：/observe 落实 ObserveRequest——mode（text/tree/tree_image
/// 响应组成）、budget_tokens（响应副本裁剪、存储帧不动）、subtree。
#[tokio::test]
async fn edge_observe_modes_budget() {
    if std::env::var("RIG_EDGE_IT").is_err() {
        eprintln!("跳过：未设置 RIG_EDGE_IT");
        return;
    }
    let _serial = SERIAL.lock().await;
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let origin = format!("http://127.0.0.1:{port}");
    let bound = serve(listener, vec![("/modes.html".into(), MODES_HTML.into())]);
    assert_eq!(bound, port);
    let edge_port = free_port();
    let _edge = spawn_edge(&[
        "--listen".into(),
        format!("127.0.0.1:{edge_port}"),
        "--url".into(),
        format!("{origin}/modes.html"),
        "--width".into(),
        "900".into(),
        "--height".into(),
        "900".into(),
    ]);
    wait_health(edge_port).await;

    let observe_with =
        |body: Value| -> (u16, Value) { http_json(edge_port, "POST", "/observe", Some(body)) };

    // 默认（tree）：全量元素 + 文本
    let (st, full) = observe_with(json!({}));
    assert_eq!(st, 200, "默认观察: {full}");
    let full_n = count_json_elements(&full["observation"]);
    assert!(full_n >= 12, "默认观察应有 12+ 元素，实际 {full_n}");
    let btn = find_element(&full["observation"], "button", "计数按钮")
        .expect("应有计数按钮")
        .clone();
    let btn_id = btn["id"].as_str().unwrap().to_string();
    let wrap = find_by_name(&full["observation"], "内容区")
        .expect("应有内容区")
        .clone();
    let wrap_id = wrap["id"].as_str().unwrap().to_string();

    // mode=text：只有文本视图，没有 observation
    let (st, v) = observe_with(json!({"mode": "text"}));
    assert_eq!(st, 200, "text 模式: {v}");
    assert!(v.get("observation").is_none(), "text 模式不应带树: {v}");
    assert!(
        v["text"].as_str().unwrap_or("").contains("计数按钮"),
        "text 模式应带文本渲染: {v}"
    );

    // budget：响应视图被裁、带裁剪说明；存储帧不动（随后点击仍成）
    let (st, v) = observe_with(json!({"budget_tokens": 60}));
    assert_eq!(st, 200, "budget 观察: {v}");
    let trimmed_n = count_json_elements(&v["observation"]);
    assert!(
        trimmed_n < full_n,
        "budget 应裁掉元素：{trimmed_n} < {full_n}"
    );
    assert!(
        v["text"].as_str().unwrap_or("").contains("裁剪响应视图"),
        "文本应带裁剪说明: {}",
        v["text"]
    );
    let frame = v["observation"]["frame_id"].as_u64().unwrap();
    let r = act(
        edge_port,
        frame,
        json!({"kind": "click", "target": {"by": "element", "id": btn_id}}),
    );
    assert_eq!(r["result"], "done", "裁剪后点击存储帧元素应成功: {r}");
    let (_, v) = observe_with(json!({}));
    assert!(
        v["text"].as_str().unwrap_or("").contains("计数:1"),
        "点击应生效（计数:1）: {}",
        v["text"]
    );

    // subtree：只回内容区子树；不存在的 id → 400
    let (st, v) = observe_with(json!({"subtree": wrap_id}));
    assert_eq!(st, 200, "subtree 观察: {v}");
    let roots = v["observation"]["elements"].as_array().unwrap();
    assert_eq!(roots.len(), 1, "subtree 应只有一个根: {v}");
    assert_eq!(roots[0]["id"], wrap_id);
    let (st, _) = observe_with(json!({"subtree": "e99999"}));
    assert_eq!(st, 400, "不存在的 subtree 应 400");

    // mode=tree_image：附编号截图（base64 PNG）
    let (st, v) = observe_with(json!({"mode": "tree_image"}));
    assert_eq!(st, 200, "tree_image 模式: {v}");
    let b64 = v["image_png_base64"].as_str().unwrap_or("");
    assert!(
        b64.starts_with("iVBOR") && b64.len() > 1000,
        "应附 PNG 截图（base64），长度 {}",
        b64.len()
    );

    // include_offscreen：默认不带视口外元素（底部按钮在 1500px
    // 间隔层之后、必在视口外）；显式 true 才带
    let (st, v) = observe_with(json!({}));
    assert_eq!(st, 200, "默认观察（离屏过滤）: {v}");
    assert!(
        find_element(&v["observation"], "button", "底部按钮").is_none(),
        "默认不应带视口外的底部按钮: {v}"
    );
    let (st, v) = observe_with(json!({"include_offscreen": true}));
    assert_eq!(st, 200, "include_offscreen 观察: {v}");
    assert!(
        find_element(&v["observation"], "button", "底部按钮").is_some(),
        "include_offscreen=true 应带底部按钮: {v}"
    );
    let frame_a = v["observation"]["frame_id"].as_u64().unwrap();

    // since_frame：点击后增量到达（基帧对齐、upsert 含新计数）；
    // act 内部刷新帧不冲掉基帧（历史池）
    let r = act(
        edge_port,
        frame_a,
        json!({"kind": "click", "target": {"by": "element", "id": btn_id}}),
    );
    assert_eq!(r["result"], "done", "增量前点击应成功: {r}");
    let (st, v) = observe_with(json!({"since_frame": frame_a, "include_offscreen": true}));
    assert_eq!(st, 200, "增量观察: {v}");
    let delta = v.get("delta").expect("since_frame 命中应回增量");
    assert_eq!(delta["base_frame"].as_u64().unwrap(), frame_a);
    assert_ne!(delta["frame_id"].as_u64().unwrap(), frame_a);
    let upserts = delta["upserts"].as_array().unwrap();
    assert!(!upserts.is_empty(), "点击后应有 upsert: {delta}");
    assert!(
        serde_json::to_string(delta).unwrap().contains("计数:2"),
        "upsert 应含新计数 计数:2: {delta}"
    );
    let (st, incompatible) =
        observe_with(json!({"since_frame": frame_a, "include_offscreen": false}));
    assert_eq!(st, 200);
    assert!(
        incompatible.get("observation").is_some() && incompatible.get("delta").is_none(),
        "輸出投影不相容應回完整觀察: {incompatible}"
    );
    // 基帧不存在 → 回完整视图；text 模式不走增量
    let (st, v) = observe_with(json!({"since_frame": 999999}));
    assert_eq!(st, 200, "未知基帧观察: {v}");
    assert!(
        v.get("delta").is_none() && v.get("observation").is_some(),
        "未知基帧应回完整视图: {v}"
    );
    let (st, v) = observe_with(json!({"since_frame": frame_a, "mode": "text"}));
    assert_eq!(st, 200, "text 模式带 since_frame: {v}");
    assert!(
        v.get("delta").is_none() && v.get("text").is_some(),
        "text 模式不走增量: {v}"
    );
}

/// 子樹投影排除密碼欄位時，HTTP 與 MCP 的 tree_image 仍以完整觀察遮罩，且編號一致。
#[tokio::test]
async fn edge_tree_image_masks_projected_out_secret_and_matches_mcp() {
    if std::env::var("RIG_EDGE_IT").is_err() {
        eprintln!("跳过：未设置 RIG_EDGE_IT");
        return;
    }
    let _serial = SERIAL.lock().await;
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let origin = format!("http://127.0.0.1:{port}");
    serve(
        listener,
        vec![("/mask.html".into(), SCREENSHOT_MASK_HTML.into())],
    );

    let edge_port = free_port();
    let mut child = EdgeProc(
        Command::new(env!("CARGO_BIN_EXE_rig-edge-browser"))
            .args([
                "--stdio",
                "--human",
                "--listen",
                &format!("127.0.0.1:{edge_port}"),
                "--url",
                &format!("{origin}/mask.html"),
                "--width",
                "900",
                "--height",
                "700",
            ])
            .process_group(0)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("啟動 MCP 邊緣"),
    );
    wait_health(edge_port).await;
    let (_, http_full) = http_json(edge_port, "POST", "/observe", Some(json!({})));
    let secret = find_element(&http_full["observation"], "textbox", "合成密碼")
        .expect("完整觀察應含合成密碼欄位");
    assert!(
        !http_full.to_string().contains("SYNTHETIC-ONLY"),
        "合成密碼不得出現在觀察: {http_full}"
    );
    let secret_bbox = secret["bbox"]
        .as_array()
        .expect("密碼欄位應有 bbox")
        .clone();
    let main_id = find_by_name(&http_full["observation"], "公開區").unwrap()["id"]
        .as_str()
        .unwrap()
        .to_string();
    let request = json!({"mode":"tree_image", "subtree":main_id});
    let (status, http_image_out) = http_json(edge_port, "POST", "/observe", Some(request.clone()));
    assert_eq!(status, 200, "HTTP tree_image 應成功: {http_image_out}");
    assert!(
        find_element(&http_image_out["observation"], "textbox", "合成密碼").is_none(),
        "子樹投影應排除密碼欄位: {http_image_out}"
    );
    let http_png = base64::engine::general_purpose::STANDARD
        .decode(
            http_image_out["image_png_base64"]
                .as_str()
                .expect("HTTP 應返回 PNG"),
        )
        .unwrap();
    let http_img = image::load_from_memory(&http_png).unwrap().to_rgba8();
    let [x1, y1, x2, y2] = secret_bbox
        .iter()
        .map(|v| v.as_f64().unwrap())
        .collect::<Vec<_>>()
        .try_into()
        .unwrap();
    let (x1, y1, x2, y2) = (x1 as u32, y1 as u32, x2 as u32, y2 as u32);
    for (left, top, right, bottom) in [(x1, y1, x2, y2), (360, 190, 540, 238)] {
        for y in top..bottom {
            for x in left..right {
                assert_eq!(
                    http_img.get_pixel(x, y).0,
                    [0, 0, 0, 255],
                    "敏感欄位像素 ({x},{y}) 必須為黑色"
                );
            }
        }
    }
    let outside = http_img
        .get_pixel(x1.saturating_sub(3), y1 + (y2 - y1) / 2)
        .0;
    assert!(
        outside[1] > 150 && outside[0] < 80,
        "敏感框外合成綠色背景應保留，避免整頁黑屏假通過: {outside:?}"
    );

    let mut stdin = child.0.stdin.take().unwrap();
    let (tx, rx) = std::sync::mpsc::channel::<String>();
    let mut reader = std::io::BufReader::new(child.0.stdout.take().unwrap());
    std::thread::spawn(move || {
        use std::io::BufRead;
        let mut line = String::new();
        loop {
            line.clear();
            if reader.read_line(&mut line).unwrap_or(0) == 0 || tx.send(line.clone()).is_err() {
                break;
            }
        }
    });
    let mut id = 0u64;
    let mut rpc = |stdin: &mut std::process::ChildStdin, method: &str, params: Value| {
        id += 1;
        writeln!(
            stdin,
            "{}",
            json!({"jsonrpc":"2.0", "id":id, "method":method, "params":params})
        )
        .unwrap();
        stdin.flush().unwrap();
        serde_json::from_str::<Value>(
            &rx.recv_timeout(Duration::from_secs(25))
                .expect("MCP 響應超時"),
        )
        .unwrap()
    };
    let _ = rpc(
        &mut stdin,
        "initialize",
        json!({"protocolVersion":"2025-03-26", "capabilities":{}, "clientInfo":{"name":"mask-it", "version":"0"}}),
    );
    writeln!(
        stdin,
        "{}",
        json!({"jsonrpc":"2.0", "method":"notifications/initialized"})
    )
    .unwrap();
    stdin.flush().unwrap();
    let result = rpc(
        &mut stdin,
        "tools/call",
        json!({"name":"observe", "arguments":request}),
    );
    let content = result["result"]["content"].as_array().unwrap();
    let mut mcp_view: Value = serde_json::from_str(content[1]["text"].as_str().unwrap()).unwrap();
    mcp_view["frame_id"] = http_image_out["observation"]["frame_id"].clone();
    assert_eq!(
        http_image_out["observation"], mcp_view,
        "相同投影的 HTTP/MCP Observation 應一致"
    );
    assert_eq!(content[2]["type"], "image");
    let mcp_png = base64::engine::general_purpose::STANDARD
        .decode(content[2]["data"].as_str().unwrap())
        .unwrap();
    let mcp_img = image::load_from_memory(&mcp_png).unwrap().to_rgba8();
    assert_eq!(http_img.dimensions(), mcp_img.dimensions());
    assert_eq!(
        http_img.as_raw(),
        mcp_img.as_raw(),
        "HTTP 與 MCP 的標記、編號、遮罩像素應一致"
    );
    let (status, http_png) = http(edge_port, "GET", "/screenshot.png", None);
    assert_eq!(status, 200);
    let standalone = rpc(
        &mut stdin,
        "tools/call",
        json!({"name":"screenshot","arguments":{}}),
    );
    let mcp_png = base64::engine::general_purpose::STANDARD
        .decode(standalone["result"]["content"][0]["data"].as_str().unwrap())
        .unwrap();
    assert_eq!(http_png, mcp_png, "獨立截圖也應共用編號與遮罩");
    drop(stdin);
}

const PERSIST_HTML: &str = r#"<!DOCTYPE html><html><head><meta charset="utf-8"><title>持久化测试页</title></head>
<body><div id="out">PENDING</div><script>
const out = document.getElementById('out');
function idbOpen() { return new Promise((res) => {
  const rq = indexedDB.open('rigdb', 1);
  rq.onupgradeneeded = () => { rq.result.createObjectStore('s'); };
  rq.onsuccess = () => res(rq.result); rq.onerror = () => res(null);
}); }
(async () => {
  const db = await idbOpen();
  if (location.search.includes('set')) {
    document.cookie = 'rigc=cv1; path=/; max-age=86400';
    localStorage.setItem('rigk', 'lv1');
    if (db) await new Promise((r) => { const p = db.transaction('s', 'readwrite').objectStore('s').put('iv1', 'k'); p.onsuccess = r; p.onerror = r; });
  }
  const ck = (document.cookie.match(/rigc=([^;]*)/) || [])[1] || '';
  const ls = localStorage.getItem('rigk') || '';
  let idb = '';
  if (db) idb = await new Promise((r) => { try { const g = db.transaction('s').objectStore('s').get('k'); g.onsuccess = () => r(g.result || ''); g.onerror = () => r(''); } catch (e) { r(''); } });
  out.textContent = 'PERSIST cookie=' + ck + ' ls=' + ls + ' idb=' + idb;
})();
</script></body></html>"#;

/// 持久化双目录（LIU 2026-11-20 定）：--profile-dir 里的 Cookie /
/// LocalStorage / IndexedDB 必须跨边缘重启存活（登录态与站点状态
/// 的最小代表集），--cache-dir 被单独使用。
#[tokio::test]
async fn edge_profile_persistence() {
    if std::env::var("RIG_EDGE_IT").is_err() {
        eprintln!("跳过：未设置 RIG_EDGE_IT");
        return;
    }
    let _serial = SERIAL.lock().await;
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let origin = format!("http://127.0.0.1:{port}");
    serve(
        listener,
        vec![("/persist.html".to_string(), PERSIST_HTML.to_string())],
    );
    let base = std::env::temp_dir().join(format!("rig-it-persist-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let profile = base.join("profile");
    let cache = base.join("cache");

    // 等 observe 文本里三类存储值齐备
    async fn wait_persist(edge_port: u16) -> bool {
        for _ in 0..60 {
            let (_, v) = http_json(edge_port, "POST", "/observe", Some(json!({"mode": "text"})));
            let s = v.to_string();
            if s.contains("cv1") && s.contains("lv1") && s.contains("iv1") {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
        false
    }

    fn file_contains(path: &std::path::Path, needle: &[u8]) -> bool {
        std::fs::read(path)
            .map(|b| b.windows(needle.len()).any(|w| w == needle))
            .unwrap_or(false)
    }

    // ── A：带 ?set 写入三类存储 ──
    let port_a = free_port();
    let mut proc_a = spawn_edge(&[
        "--listen".into(),
        format!("127.0.0.1:{port_a}"),
        "--url".into(),
        format!("{origin}/persist.html?set"),
        "--profile-dir".into(),
        profile.display().to_string(),
        "--cache-dir".into(),
        cache.display().to_string(),
    ]);
    wait_health(port_a).await;
    assert!(
        wait_persist(port_a).await,
        "A 轮写入后 observe 应读到 cookie/localStorage/IndexedDB 三值"
    );
    // 优雅停机（生产口径）：SIGTERM 只发给边缘本体——边缘须请
    // Chromium 落盘关闭（Browser.close）再退出；整组强杀会丢
    // 未提交的 cookie/localStorage（首跑实测教训）
    let _ = Command::new("kill")
        .args(["-TERM", &proc_a.0.id().to_string()])
        .status();
    let mut exited = false;
    for _ in 0..40 {
        if matches!(proc_a.0.try_wait(), Ok(Some(_))) {
            exited = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    assert!(exited, "边缘应在 SIGTERM 后优雅退出");
    // 落盘实据：cookie 名与 localStorage 值应已进磁盘文件
    let dft = profile.join("Default");
    assert!(
        file_contains(&dft.join("Cookies"), b"rigc"),
        "优雅关闭后 cookie 应已落盘"
    );
    assert!(
        file_contains(&dft.join("LocalStorage"), b"lv1")
            || file_contains(&dft.join("LocalStorage-wal"), b"lv1"),
        "优雅关闭后 localStorage 应已落盘"
    );
    drop(proc_a);
    tokio::time::sleep(Duration::from_millis(800)).await;
    assert!(dft.exists(), "profile 目录应已建出 Default");
    assert!(cache.exists(), "cache 目录应已被使用");

    // ── B：同双目录重启、无 ?set——三值必须从盘上恢复 ──
    let port_b = free_port();
    let proc_b = spawn_edge(&[
        "--listen".into(),
        format!("127.0.0.1:{port_b}"),
        "--url".into(),
        format!("{origin}/persist.html"),
        "--profile-dir".into(),
        profile.display().to_string(),
        "--cache-dir".into(),
        cache.display().to_string(),
    ]);
    wait_health(port_b).await;
    assert!(
        wait_persist(port_b).await,
        "B 轮（同 profile 重启）应从盘恢复三值——登录态持久化失效"
    );
    drop(proc_b);
    let _ = std::fs::remove_dir_all(&base);
}

/// 双击 fixture：#tgt 统计 dblclick 次数并写回 #out
const CLICK_HTML: &str = r#"<!doctype html><meta charset="utf-8"><title>click</title>
<div id="tgt" style="width:300px;height:200px;background:#ccc">target</div>
<div id="out">dc=0</div>
<script>
document.getElementById('tgt').addEventListener('dblclick', () => {
  const o = document.getElementById('out');
  o.textContent = 'dc=' + (parseInt(o.textContent.slice(3), 10) + 1);
});
</script>"#;

/// 双击 clickCount 透传 + 内部页 DOM 把关（LIU 2026-11-20 第三轮
/// 实测反馈）：①②观看端 count=2 经视频/DOM 两条输入路径进 CDP
/// clickCount，远端页面须收到真 dblclick（旧实现恒 1、且观看页
/// 自绑 dblclick 全屏劫持）；③切到 chrome://settings 后 DOM
/// 通道只报内部页状态、不产该页快照（WebUI 界面在 shadow DOM
/// 里，镜像只有空壳），观看端据此回落视频模式。
#[tokio::test]
async fn edge_internal_gate_and_dblclick() {
    if std::env::var("RIG_EDGE_IT").is_err() {
        eprintln!("跳过：未设置 RIG_EDGE_IT");
        return;
    }
    let _serial = SERIAL.lock().await;
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let origin = format!("http://127.0.0.1:{port}");
    serve(
        listener,
        vec![("/click.html".to_string(), CLICK_HTML.to_string())],
    );
    let edge_port = free_port();
    let proc = spawn_edge(&[
        "--listen".into(),
        format!("127.0.0.1:{edge_port}"),
        "--url".into(),
        format!("{origin}/click.html"),
        "--human".into(),
        "--allow-origin".into(),
        origin.clone(),
    ]);
    let _ = &proc;
    wait_health(edge_port).await;

    // 校验预言机用 DOM 快照本身，绝不能用 /observe 轮询：
    // observe 是 AI tools/call，按 §7.1 其在途+保持窗内人手势
    // 被遮蔽——拿它当判据会把要验的人路径点击自己锁死
    // （2026-11-20 实测：observe 后点击全灭、导航后恢复）
    async fn next_snap_soft(ws: &mut WsStream, within: Duration) -> Option<Value> {
        let deadline = Instant::now() + within;
        while Instant::now() < deadline {
            match tokio::time::timeout(Duration::from_secs(1), ws.next()).await {
                Ok(Some(Ok(tokio_tungstenite::tungstenite::Message::Text(t)))) => {
                    if let Ok(v) = serde_json::from_str::<Value>(&t) {
                        if v["type"] == "dom-snapshot" {
                            return Some(v);
                        }
                    }
                }
                Ok(Some(Ok(_))) => {}
                _ => {}
            }
        }
        None
    }

    use futures_util::{SinkExt, StreamExt};
    let (mut ws, _) = tokio_tungstenite::connect_async(format!("ws://127.0.0.1:{edge_port}/ws"))
        .await
        .expect("人通道 WS 应可连接");
    macro_rules! ws_send {
        ($v:expr) => {
            ws.send(tokio_tungstenite::tungstenite::Message::Text(
                $v.to_string().into(),
            ))
            .await
            .unwrap()
        };
    }
    // 一次完整双击：count 1 的按下/抬起 + count 2 的按下/抬起
    macro_rules! dblclick {
        ($dom:expr) => {
            for c in [1, 2] {
                ws_send!(json!({"type": "mouse", "dom": $dom, "action": "down", "x": 100, "y": 100, "button": 0, "buttons": 1, "count": c}));
                ws_send!(json!({"type": "mouse", "dom": $dom, "action": "up", "x": 100, "y": 100, "button": 0, "buttons": 0, "count": c}));
            }
        };
    }

    // ── ① 视频路径双击 ──
    ws_send!(json!({"type": "dom-start"}));
    // 基线：轮询快照直到页面渲染出 dc=0（兼作就绪凭据）
    let mut base_ok = false;
    for _ in 0..12 {
        if let Some(s) = next_snap_soft(&mut ws, Duration::from_secs(3)).await {
            if s.to_string().contains("dc=0") {
                base_ok = true;
                break;
            }
        }
        ws_send!(json!({"type": "dom-resync"}));
    }
    assert!(base_ok, "基线快照应含 dc=0（页面已渲染）");
    dblclick!(false);
    let mut ok1 = false;
    for _ in 0..12 {
        tokio::time::sleep(Duration::from_millis(500)).await;
        ws_send!(json!({"type": "dom-resync"}));
        if let Some(s) = next_snap_soft(&mut ws, Duration::from_secs(3)).await {
            if s.to_string().contains("dc=1") {
                ok1 = true;
                break;
            }
        }
    }
    assert!(ok1, "视频路径 count=2 应让远端页面收到 dblclick");

    // ── ② DOM 路径双击 ──
    dblclick!(true);
    let mut ok2 = false;
    for _ in 0..12 {
        tokio::time::sleep(Duration::from_millis(500)).await;
        ws_send!(json!({"type": "dom-resync"}));
        if let Some(s) = next_snap_soft(&mut ws, Duration::from_secs(3)).await {
            if s.to_string().contains("dc=2") {
                ok2 = true;
                break;
            }
        }
    }
    assert!(ok2, "DOM 路径 count=2 应让远端页面收到 dblclick");

    // ── ③ 内部页把关 ──
    ws_send!(json!({"type": "chrome", "action": "open-internal", "url": "chrome://settings"}));
    tokio::time::sleep(Duration::from_secs(2)).await;
    ws_send!(json!({"type": "dom-resync"}));
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut saw_internal_state = false;
    let mut saw_internal_snap = false;
    while Instant::now() < deadline && !saw_internal_state {
        match tokio::time::timeout(Duration::from_secs(2), ws.next()).await {
            Ok(Some(Ok(tokio_tungstenite::tungstenite::Message::Text(t)))) => {
                if let Ok(v) = serde_json::from_str::<Value>(&t) {
                    if v["type"] == "dom-state"
                        && v["reason"].as_str().is_some_and(|r| r.contains("内部页面"))
                    {
                        saw_internal_state = true;
                    }
                    if v["type"] == "dom-snapshot"
                        && v["url"]
                            .as_str()
                            .is_some_and(|u| u.starts_with("chrome://"))
                    {
                        saw_internal_snap = true;
                    }
                }
            }
            Ok(Some(Ok(_))) => {}
            _ => {}
        }
    }
    assert!(saw_internal_state, "内部页应收到 DOM 把关状态（回落视频）");
    assert!(!saw_internal_snap, "内部页不应产出 DOM 快照（镜像空壳）");
    drop(proc);
}

/// D01/D02 回归（报告测试#5/#6）：同一连接重复 dom-start 只订阅一次
/// （自动感知 + 前台显示共用一份）；发送侧先结束/直接断开不断开清理——
/// 订阅释放只在父作用域，不泄漏名额、不倒扣。
#[tokio::test]
async fn edge_dom_sub_idempotent_and_cleanup() {
    if std::env::var("RIG_EDGE_IT").is_err() {
        eprintln!("跳过：未设置 RIG_EDGE_IT");
        return;
    }
    let _serial = SERIAL.lock().await;
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let origin = format!("http://127.0.0.1:{port}");
    serve(
        listener,
        vec![("/dom.html".to_string(), DOM_HTML.to_string())],
    );
    let edge_port = free_port();
    let mut proc = spawn_edge(&[
        "--listen".into(),
        format!("127.0.0.1:{edge_port}"),
        "--url".into(),
        format!("{origin}/dom.html"),
        "--human".into(),
        "--width".into(),
        "900".into(),
        "--height".into(),
        "700".into(),
    ]);
    wait_health(edge_port).await;
    let _ = &mut proc;

    async fn wait_viewers(edge_port: u16, want: u64, what: &str) {
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            let v = http_json(edge_port, "GET", "/health", None).1["dom_viewers"]
                .as_u64()
                .unwrap_or(u64::MAX);
            if v == want {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "{what}：dom_viewers 应为 {want}，实际 {v}"
            );
            tokio::time::sleep(Duration::from_millis(300)).await;
        }
    }

    use futures_util::SinkExt;
    let (mut ws, _) = tokio_tungstenite::connect_async(format!("ws://127.0.0.1:{edge_port}/ws"))
        .await
        .expect("人通道 WS 应可连接");
    wait_viewers(edge_port, 0, "初始").await;
    // 重复 start 只订阅一次
    for _ in 0..2 {
        ws.send(tokio_tungstenite::tungstenite::Message::Text(
            r#"{"type":"dom-start"}"#.into(),
        ))
        .await
        .unwrap();
    }
    wait_viewers(edge_port, 1, "重复 dom-start").await;
    // 配对 stop 回零；多余 stop 不倒扣
    ws.send(tokio_tungstenite::tungstenite::Message::Text(
        r#"{"type":"dom-stop"}"#.into(),
    ))
    .await
    .unwrap();
    wait_viewers(edge_port, 0, "配对 dom-stop").await;
    ws.send(tokio_tungstenite::tungstenite::Message::Text(
        r#"{"type":"dom-stop"}"#.into(),
    ))
    .await
    .unwrap();
    tokio::time::sleep(Duration::from_millis(500)).await;
    wait_viewers(edge_port, 0, "多余 dom-stop 不得倒扣").await;
    // 直接断开（不发 dom-stop）：父作用域必须回收名额
    ws.send(tokio_tungstenite::tungstenite::Message::Text(
        r#"{"type":"dom-start"}"#.into(),
    ))
    .await
    .unwrap();
    wait_viewers(edge_port, 1, "重入").await;
    drop(ws);
    wait_viewers(edge_port, 0, "断开连接后名额应回收").await;
    drop(proc);
}

/// 页面开新标签（target=_blank / window.open）会抢走前台，观看标签变后台，
/// Chrome 对后台页不再出 screencast 帧，观看画面冻结。帧泵巡检发现新标签后
/// 必须把观看标签切回前台：开标签之后帧仍持续输出（动画页，帧龄应很小）。
#[tokio::test]
async fn edge_popup_keeps_view_frames() {
    if std::env::var("RIG_EDGE_IT").is_err() {
        eprintln!("跳过：未设置 RIG_EDGE_IT");
        return;
    }
    let _serial = SERIAL.lock().await;
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let origin = format!("http://127.0.0.1:{port}");
    let page = r##"<!DOCTYPE html><html><head><meta charset="utf-8"><title>popup</title></head>
<body style="margin:0"><a href="/other.html" target="_blank" style="display:block;width:400px;height:100px;background:#fa0">开新标签</a>
<canvas id="c" width="600" height="300"></canvas>
<script>const x=document.getElementById('c').getContext('2d');let i=0;setInterval(()=>{x.fillStyle='hsl('+(i++*5%360)+',80%,50%)';x.fillRect(0,0,600,300)},33)</script>
</body></html>"##;
    serve(
        listener,
        vec![
            ("/popup.html".to_string(), page.to_string()),
            (
                "/other.html".to_string(),
                r##"<html><body><canvas id="c" width="600" height="300"></canvas><script>const x=document.getElementById('c').getContext('2d');let i=0;setInterval(()=>{x.fillStyle='hsl('+(i++*7%360)+',80%,50%)';x.fillRect(0,0,600,300)},33)</script></body></html>"##.to_string(),
            ),
        ],
    );
    let edge_port = free_port();
    let mut proc = spawn_edge(&[
        "--listen".into(),
        format!("127.0.0.1:{edge_port}"),
        "--url".into(),
        format!("{origin}/popup.html"),
        "--human".into(),
        "--width".into(),
        "900".into(),
        "--height".into(),
        "700".into(),
    ]);
    wait_health(edge_port).await;
    let _ = &mut proc;

    async fn frames_sent(edge_port: u16) -> (u64, f64) {
        let h = http_json(edge_port, "GET", "/health", None).1;
        (
            h["frames"]["sent"].as_u64().unwrap_or(0),
            h["last_frame_age_ms"].as_f64().unwrap_or(f64::MAX),
        )
    }
    use futures_util::SinkExt;
    let (mut ws, _) = tokio_tungstenite::connect_async(format!("ws://127.0.0.1:{edge_port}/ws"))
        .await
        .expect("人通道 WS 应可连接");
    // 实际观看后才有采集需求；先确认原动画标签已开始出帧。
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if frames_sent(edge_port).await.0 > 0 {
            break;
        }
        assert!(Instant::now() < deadline, "开标签前应已有帧");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    for (action, buttons) in [("move", 0), ("down", 1), ("up", 0)] {
        ws.send(tokio_tungstenite::tungstenite::Message::Text(
            format!(
                r#"{{"type":"mouse","action":"{action}","button":0,"buttons":{buttons},"count":1,"x":100,"y":40}}"#
            )
            .into(),
        ))
        .await
        .unwrap();
    }
    // 新标签必须真的开出来、自己加载（不停在 about:blank 等手动刷新）、
    // 且观看画面跟了过去（active=新标签）：看观看通道的 chrome-state
    {
        use futures_util::StreamExt;
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            assert!(
                !left.is_zero(),
                "点击链接后新标签应加载 other.html 并成为观看标签"
            );
            let Ok(Some(Ok(msg))) = tokio::time::timeout(left, ws.next()).await else {
                continue;
            };
            if let tokio_tungstenite::tungstenite::Message::Text(t) = msg {
                if let Ok(v) = serde_json::from_str::<Value>(&t) {
                    if v["type"] == "chrome-state" {
                        let tabs = v["tabs"].as_array().cloned().unwrap_or_default();
                        let loaded = tabs.iter().position(|t| {
                            t["url"]
                                .as_str()
                                .is_some_and(|u| u.ends_with("/other.html"))
                        });
                        if loaded.is_some() && loaded == v["active"].as_u64().map(|a| a as usize) {
                            break;
                        }
                    }
                }
            }
        }
    }
    // 巡检周期 600ms；给 3s 收敛，之后帧必须持续增长、帧龄很小
    tokio::time::sleep(Duration::from_secs(3)).await;
    let (a, _) = frames_sent(edge_port).await;
    tokio::time::sleep(Duration::from_secs(2)).await;
    let (b, age) = frames_sent(edge_port).await;
    assert!(
        b > a + 10 && age < 1500.0,
        "跟随新标签后观看画面应持续出帧：sent {a}→{b}，帧龄 {age}ms"
    );
}

/// 回归：同一批突变里「先把父挂进文档、再往父里加子」（YouTube 字幕即此模式）。
/// ser(父) 读的是回调时的最终 DOM，已含子节点；子节点那条 childList 记录若再发
/// 一次 add，镜像里就重复一份（实测字幕逐字翻倍）。断言：整个增量流里目标文本
/// 只在一个 add 树里出现一次。
#[tokio::test]
async fn edge_dom_no_duplicate_adds() {
    if std::env::var("RIG_EDGE_IT").is_err() {
        eprintln!("跳过：未设置 RIG_EDGE_IT");
        return;
    }
    let _serial = SERIAL.lock().await;
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let origin = format!("http://127.0.0.1:{port}");
    let page = r##"<!DOCTYPE html><html><head><meta charset="utf-8"><title>dup</title></head>
<body style="margin:0"><button id="b" style="position:absolute;left:0;top:0;width:120px;height:40px"
 onclick="const c=document.getElementById('box');const p=document.createElement('div');c.appendChild(p);const s=document.createElement('span');s.textContent='UNIQ-TEXT';p.appendChild(s);">go</button>
<div id="box" style="position:absolute;top:60px"></div></body></html>"##;
    serve(listener, vec![("/dup.html".to_string(), page.to_string())]);
    let edge_port = free_port();
    let mut proc = spawn_edge(&[
        "--listen".into(),
        format!("127.0.0.1:{edge_port}"),
        "--url".into(),
        format!("{origin}/dup.html"),
        "--human".into(),
        "--width".into(),
        "900".into(),
        "--height".into(),
        "700".into(),
    ]);
    wait_health(edge_port).await;
    let _ = &mut proc;
    tokio::time::sleep(Duration::from_millis(800)).await;

    use futures_util::SinkExt;
    let (mut ws, _) = tokio_tungstenite::connect_async(format!("ws://127.0.0.1:{edge_port}/ws"))
        .await
        .expect("人通道 WS 应可连接");
    ws.send(tokio_tungstenite::tungstenite::Message::Text(
        json!({"type": "dom-start"}).to_string().into(),
    ))
    .await
    .unwrap();
    let _snap = next_dom(&mut ws, "dom-snapshot", Duration::from_secs(20)).await;
    for action in ["down", "up"] {
        ws.send(tokio_tungstenite::tungstenite::Message::Text(
            json!({"type": "mouse", "dom": true, "action": action, "x": 50, "y": 20})
                .to_string()
                .into(),
        ))
        .await
        .unwrap();
    }
    fn count_text(n: &Value, needle: &str) -> usize {
        let mut c = usize::from(n["t"].as_str() == Some(needle));
        if let Some(a) = n["c"].as_array() {
            c += a.iter().map(|x| count_text(x, needle)).sum::<usize>();
        }
        c
    }
    // 收满 2s 内的全部差分，统计 UNIQ-TEXT 在所有 add 子树里共出现几次
    let got = collect_deltas_until(&mut ws, |_| false, Duration::from_secs(3)).await;
    let total: usize = got
        .iter()
        .filter(|(_, op)| op["o"] == "add")
        .map(|(_, op)| count_text(&op["n"], "UNIQ-TEXT"))
        .sum();
    assert_eq!(
        total, 1,
        "UNIQ-TEXT 应只被 add 一次，实际 {total}；差分: {got:?}"
    );
}

/// 快照与增量并发（反复 dom-resync + 页面持续突变）：按观看端同样的规则
/// 重放（snapshot 重建、seq 连续才应用 delta），同一 id 不得在镜像里同时
/// 出现两份。无「快照已含的旧增量不重放」的保护时，此测试失败。
#[tokio::test(flavor = "multi_thread")]
async fn edge_dom_resync_under_mutation_no_dup() {
    if std::env::var("RIG_EDGE_IT").is_err() {
        eprintln!("跳过：未设置 RIG_EDGE_IT");
        return;
    }
    let _serial = SERIAL.lock().await;
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let origin = format!("http://127.0.0.1:{port}");
    let page = r##"<!DOCTYPE html><html><head><meta charset="utf-8"><title>churn</title></head>
<body><div id="box"></div><div id="big"></div><script>
for(let k=0;k<6000;k++){const d=document.createElement('div');d.textContent='x'+k;document.getElementById('big').appendChild(d);}
let n=0;const box=document.getElementById('box');
setInterval(()=>{const p=document.createElement('p');const b=document.createElement('b');
 b.textContent='t'+(n++);p.appendChild(b);box.appendChild(p);
 if(box.children.length>20)box.removeChild(box.firstChild);},4);
</script></body></html>"##;
    serve(
        listener,
        vec![("/churn.html".to_string(), page.to_string())],
    );
    let edge_port = free_port();
    let mut proc = spawn_edge(&[
        "--listen".into(),
        format!("127.0.0.1:{edge_port}"),
        "--url".into(),
        format!("{origin}/churn.html"),
        "--human".into(),
    ]);
    wait_health(edge_port).await;
    let _ = &mut proc;
    tokio::time::sleep(Duration::from_millis(800)).await;

    use futures_util::SinkExt;
    let (mut ws, _) = tokio_tungstenite::connect_async(format!("ws://127.0.0.1:{edge_port}/ws"))
        .await
        .expect("人通道 WS 应可连接");
    let send = |t: &str| {
        tokio_tungstenite::tungstenite::Message::Text(json!({"type": t}).to_string().into())
    };
    ws.send(send("dom-start")).await.unwrap();

    // id -> 子 id；present 为镜像里当前存在的 id 集合
    fn add_tree(n: &Value, present: &mut std::collections::HashSet<u64>, dup: &mut Vec<u64>) {
        if let Some(i) = n["i"].as_u64() {
            if !present.insert(i) {
                dup.push(i);
            }
        }
        for c in n["c"].as_array().into_iter().flatten() {
            add_tree(c, present, dup);
        }
    }
    fn drop_ids(n: &Value, present: &mut std::collections::HashSet<u64>) {
        if let Some(i) = n["i"].as_u64() {
            present.remove(&i);
        }
        for c in n["c"].as_array().into_iter().flatten() {
            drop_ids(c, present);
        }
    }
    let mut present = std::collections::HashSet::new();
    let mut dup: Vec<u64> = Vec::new();
    let mut seq = 0u64;
    let mut synced = false;
    // rm 需要子树：用 id -> 节点树的映射
    let mut nodes: std::collections::HashMap<u64, Value> = std::collections::HashMap::new();
    fn index(n: &Value, m: &mut std::collections::HashMap<u64, Value>) {
        if let Some(i) = n["i"].as_u64() {
            m.insert(i, n.clone());
        }
        for c in n["c"].as_array().into_iter().flatten() {
            index(c, m);
        }
    }
    let end = Instant::now() + Duration::from_secs(8);
    let mut next_resync = Instant::now() + Duration::from_millis(300);
    while Instant::now() < end {
        if Instant::now() >= next_resync {
            ws.send(send("dom-resync")).await.unwrap();
            next_resync = Instant::now() + Duration::from_millis(250);
        }
        let Ok(Some(Ok(tokio_tungstenite::tungstenite::Message::Text(t)))) =
            tokio::time::timeout(Duration::from_millis(200), ws.next()).await
        else {
            continue;
        };
        let Ok(v) = serde_json::from_str::<Value>(&t) else {
            continue;
        };
        match v["type"].as_str() {
            Some("dom-snapshot") => {
                present.clear();
                nodes.clear();
                add_tree(&v["tree"], &mut present, &mut dup);
                index(&v["tree"], &mut nodes);
                seq = v["seq"].as_u64().unwrap_or(0);
                synced = true;
            }
            Some("dom-delta") if synced => {
                let s = v["seq"].as_u64().unwrap_or(0);
                if s != seq + 1 {
                    synced = false; // 观看端此时会请求重同步并等新快照
                    continue;
                }
                seq = s;
                for op in v["ops"].as_array().into_iter().flatten() {
                    match op["o"].as_str() {
                        Some("add") => {
                            add_tree(&op["n"], &mut present, &mut dup);
                            index(&op["n"], &mut nodes);
                        }
                        Some("rm") => {
                            if let Some(n) = op["i"].as_u64().and_then(|i| nodes.get(&i).cloned()) {
                                drop_ids(&n, &mut present);
                            }
                        }
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }
    assert!(dup.is_empty(), "镜像里出现重复 id：{dup:?}");
}

/// 两种显示模式的 WS 鼠标事件都应保留按钮、点按和修饰键语义。
#[tokio::test]
async fn edge_ws_mouse_button_and_modifier_semantics() {
    if std::env::var("RIG_EDGE_IT").is_err() {
        eprintln!("跳过：未设置 RIG_EDGE_IT");
        return;
    }
    let _serial = SERIAL.lock().await;
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let page_port = listener.local_addr().unwrap().port();
    let origin = format!("http://127.0.0.1:{page_port}");
    let html = r#"<!doctype html><meta charset="utf-8"><title>MouseReady</title><body style="margin:0">
<div id="target" style="width:500px;height:400px">target</div><pre id="log"></pre>
<script>
const log=[]; for (const type of ['mousedown','mouseup','click','dblclick','contextmenu','auxclick','mousemove','wheel'])
  document.addEventListener(type,e=>{log.push({type:e.type,button:e.button,buttons:e.buttons,alt:e.altKey,ctrl:e.ctrlKey,meta:e.metaKey,shift:e.shiftKey});document.title=JSON.stringify(log);document.querySelector('#log').textContent=JSON.stringify(log)});
</script></body>"#;
    serve(
        listener,
        vec![("/mouse.html".to_string(), html.to_string())],
    );
    let edge_port = free_port();
    let _proc = spawn_edge(&[
        "--listen".into(),
        format!("127.0.0.1:{edge_port}"),
        "--url".into(),
        format!("{origin}/mouse.html"),
        "--human".into(),
        "--width".into(),
        "700".into(),
        "--height".into(),
        "500".into(),
    ]);
    wait_health(edge_port).await;
    use futures_util::SinkExt;
    let (mut ws, _) = tokio_tungstenite::connect_async(format!("ws://127.0.0.1:{edge_port}/ws"))
        .await
        .unwrap();
    macro_rules! send {
        ($v:expr) => {
            ws.send(tokio_tungstenite::tungstenite::Message::Text(
                $v.to_string().into(),
            ))
            .await
            .unwrap()
        };
    }
    send!(json!({"type":"dom-start"}));
    let ready = next_dom(&mut ws, "dom-snapshot", Duration::from_secs(20)).await;
    assert_eq!(
        ready["title"], "MouseReady",
        "fixture must be loaded before input: {ready}"
    );
    tokio::time::sleep(Duration::from_millis(800)).await;
    let mut previous = 0;
    for dom in [false, true] {
        // Press/release left with all four modifier bits; click count 2 yields dblclick.
        send!(
            json!({"type":"mouse","dom":dom,"action":"down","x":100,"y":100,"button":0,"buttons":1,"count":2,"modifiers":15})
        );
        send!(
            json!({"type":"mouse","dom":dom,"action":"up","x":100,"y":100,"button":0,"buttons":0,"count":2,"modifiers":15})
        );
        // Secondary and middle buttons exercise native contextmenu and auxclick events.
        for (button, mask) in [(2, 2), (1, 4)] {
            send!(
                json!({"type":"mouse","dom":dom,"action":"down","x":100,"y":100,"button":button,"buttons":mask,"count":1})
            );
            send!(
                json!({"type":"mouse","dom":dom,"action":"up","x":100,"y":100,"button":button,"buttons":0,"count":1})
            );
        }
        // Hold left + right, release right, then move with left still held.
        send!(
            json!({"type":"mouse","dom":dom,"action":"down","x":100,"y":100,"button":0,"buttons":1,"count":1})
        );
        send!(
            json!({"type":"mouse","dom":dom,"action":"down","x":100,"y":100,"button":2,"buttons":3,"count":1})
        );
        send!(
            json!({"type":"mouse","dom":dom,"action":"up","x":100,"y":100,"button":2,"buttons":1,"count":1})
        );
        send!(
            json!({"type":"mouse","dom":dom,"action":"move","x":110,"y":110,"button":0,"buttons":1})
        );
        send!(
            json!({"type":"mouse","dom":dom,"action":"up","x":110,"y":110,"button":0,"buttons":0,"count":1})
        );
        // An unmatched release must not synthesize a mouseup.
        send!(
            json!({"type":"mouse","dom":dom,"action":"up","x":100,"y":100,"button":0,"buttons":0,"count":1})
        );
        send!(json!({"type":"wheel","dom":dom,"x":100,"y":100,"dx":0,"dy":40,"modifiers":8}));
        let deadline = Instant::now() + Duration::from_secs(8);
        let log = loop {
            send!(json!({"type":"dom-resync"}));
            let snap = next_dom(&mut ws, "dom-snapshot", Duration::from_secs(3)).await;
            if let Some(log) = snap["title"]
                .as_str()
                .and_then(|title| serde_json::from_str::<Vec<Value>>(title).ok())
            {
                if log.iter().skip(previous).any(|e| e["type"] == "wheel") {
                    break log;
                }
            }
            assert!(
                Instant::now() < deadline,
                "dom={dom}: missing wheel and/or mouse events: {snap}"
            );
        };
        let events = &log[previous..];
        let pairs = |kind: &str| {
            events
                .iter()
                .filter(|e| e["type"] == kind)
                .map(|e| {
                    (
                        e["button"].as_i64().unwrap(),
                        e["buttons"].as_u64().unwrap(),
                    )
                })
                .collect::<Vec<_>>()
        };
        assert_eq!(
            pairs("mousedown"),
            [(0, 1), (2, 2), (1, 4), (0, 1), (2, 3)],
            "dom={dom}"
        );
        assert_eq!(
            pairs("mouseup"),
            [(0, 0), (2, 0), (1, 0), (2, 1), (0, 0)],
            "dom={dom}"
        );
        assert_eq!(pairs("mousemove"), [(0, 1)], "dom={dom}");
        assert_eq!(pairs("contextmenu"), [(2, 2), (2, 3)], "dom={dom}");
        assert!(pairs("auxclick").contains(&(1, 0)), "dom={dom}");
        assert!(pairs("click").contains(&(0, 0)), "dom={dom}");
        let double = events
            .iter()
            .find(|e| e["type"] == "dblclick")
            .expect("left dblclick");
        for key in ["alt", "ctrl", "meta", "shift"] {
            assert_eq!(double[key], true, "dom={dom}: {double}");
        }
        let wheel = events.iter().find(|e| e["type"] == "wheel").unwrap();
        assert_eq!(wheel["shift"], true, "dom={dom}: {wheel}");
        previous = log.len();
    }
}

/// 两种显示模式的键盘元数据、组合键和文字输入应一致。
#[tokio::test]
async fn edge_ws_keyboard_metadata_and_text_safety() {
    if std::env::var("RIG_EDGE_IT").is_err() {
        eprintln!("跳过：未设置 RIG_EDGE_IT");
        return;
    }
    let _serial = SERIAL.lock().await;
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let page_port = listener.local_addr().unwrap().port();
    let origin = format!("http://127.0.0.1:{page_port}");
    let html = r#"<!doctype html><meta charset="utf-8"><title>KeyboardReady</title><input id="field" autofocus><script>
const log=[]; const field=document.querySelector('#field');
field.addEventListener('keydown',e=>{if(['Numpad1','Semicolon','F6','F7','F8'].includes(e.code))e.preventDefault()},true);
for(const type of ['keydown','keyup','keypress','beforeinput','input']) field.addEventListener(type,e=>{log.push({type:e.type,key:e.key,code:e.code,keyCode:e.keyCode,location:e.location,repeat:e.repeat,shift:e.shiftKey,alt:e.altKey,ctrl:e.ctrlKey,meta:e.metaKey,data:e.data??null,value:field.value});document.title=JSON.stringify(log)});
field.addEventListener('keyup',e=>{if(e.code==='F7'||e.code==='F8')field.value=''});
</script>"#;
    serve(
        listener,
        vec![("/keyboard.html".to_string(), html.to_string())],
    );
    let edge_port = free_port();
    let _proc = spawn_edge(&[
        "--listen".into(),
        format!("127.0.0.1:{edge_port}"),
        "--url".into(),
        format!("{origin}/keyboard.html"),
        "--human".into(),
        "--width".into(),
        "700".into(),
        "--height".into(),
        "500".into(),
    ]);
    wait_health(edge_port).await;
    use futures_util::SinkExt;
    let (mut ws, _) = tokio_tungstenite::connect_async(format!("ws://127.0.0.1:{edge_port}/ws"))
        .await
        .unwrap();
    macro_rules! send {
        ($v:expr) => {
            ws.send(tokio_tungstenite::tungstenite::Message::Text(
                $v.to_string().into(),
            ))
            .await
            .unwrap()
        };
    }
    send!(json!({"type":"dom-start"}));
    let ready = next_dom(&mut ws, "dom-snapshot", Duration::from_secs(20)).await;
    assert_eq!(
        ready["title"], "KeyboardReady",
        "fixture not ready: {ready}"
    );
    tokio::time::sleep(Duration::from_millis(500)).await;

    let mut failures = Vec::new();
    let mut previous = 0;
    for dom in [false, true] {
        let send_key =
            |action: &str, key: &str, code: &str, modifiers: u8, location: u8, repeat: bool| {
                json!({"type":"key","dom":dom,"action":action,"key":key,"code":code,
                "modifiers":modifiers,"location":location,"repeat":repeat})
            };
        // Shortcut modifiers stay on the event while Shift+A inserts exactly once.
        for (key, code, mods) in [
            ("A", "KeyA", 8),
            ("a", "KeyA", 1),
            ("b", "KeyB", 2),
            ("c", "KeyC", 4),
        ] {
            send!(send_key("down", key, code, mods, 0, false));
            send!(send_key("up", key, code, mods, 0, false));
        }
        send!(send_key("down", "q", "KeyQ", 3, 0, false));
        send!(send_key("up", "q", "KeyQ", 3, 0, false));
        let mut alt_graph = send_key("down", "@", "KeyQ", 3, 0, false);
        alt_graph["altGraph"] = json!(true);
        send!(alt_graph);
        send!(send_key("up", "@", "KeyQ", 3, 0, false));
        for (key, location, repeat) in [
            ("Numpad1", 3, true),
            ("Semicolon", 0, false),
            ("F6", 0, false),
        ] {
            let value = match key {
                "Numpad1" => "1",
                "Semicolon" => ";",
                _ => key,
            };
            send!(send_key("down", value, key, 0, location, repeat));
            send!(send_key("up", value, key, 0, location, false));
        }
        send!(send_key("down", "Shift", "ShiftRight", 8, 2, false));
        send!(json!({"type":"input-reset"}));
        let end = if dom { "F8" } else { "F7" };
        send!(send_key("down", end, end, 0, 0, false));
        send!(send_key("up", end, end, 0, 0, false));

        let deadline = Instant::now() + Duration::from_secs(8);
        let log = loop {
            send!(json!({"type":"dom-resync"}));
            let snap = next_dom(&mut ws, "dom-snapshot", Duration::from_secs(3)).await;
            if let Some(log) = snap["title"]
                .as_str()
                .and_then(|s| serde_json::from_str::<Vec<Value>>(s).ok())
            {
                if log
                    .iter()
                    .skip(previous)
                    .any(|e| e["key"] == end && e["type"] == "keyup")
                {
                    break log;
                }
            }
            assert!(
                Instant::now() < deadline,
                "dom={dom}: keyboard end marker not observed: {snap}"
            );
        };
        let events = &log[previous..];
        previous = log.len();
        let downs = events
            .iter()
            .filter(|e| e["type"] == "keydown")
            .collect::<Vec<_>>();
        for (key, code, key_code, location, repeat) in [
            ("1", "Numpad1", 97, 3, true),
            (";", "Semicolon", 186, 0, false),
            ("F6", "F6", 117, 0, false),
        ] {
            let event = downs
                .iter()
                .find(|e| e["key"] == key)
                .expect("keydown record");
            for (field, expected) in [
                ("code", json!(code)),
                ("keyCode", json!(key_code)),
                ("location", json!(location)),
                ("repeat", json!(repeat)),
            ] {
                if event[field] != expected {
                    failures.push(format!(
                        "dom={dom} {key} {field}: expected {expected}, got {}",
                        event[field]
                    ));
                }
            }
        }
        for (key, field) in [("a", "alt"), ("b", "ctrl"), ("c", "meta")] {
            let event = downs.iter().find(|e| e["key"] == key).unwrap();
            if event[field] != true {
                failures.push(format!(
                    "dom={dom}: {key} should carry {field} modifier: {event}"
                ));
            }
        }
        assert!(
            events
                .iter()
                .any(|e| e["type"] == "keyup" && e["code"] == "ShiftRight" && e["location"] == 2),
            "dom={dom}: reset releases ShiftRight"
        );
        let shift_text = events
            .iter()
            .filter(|e| e["type"] == "input" && e["data"] == "A")
            .count();
        if shift_text != 1 {
            failures.push(format!(
                "dom={dom}: Shift text should produce one input event, got {shift_text}; {log:?}"
            ));
        }
        let final_value = events
            .iter()
            .rev()
            .find(|e| e["type"] == "input")
            .and_then(|e| e["value"].as_str())
            .unwrap_or("");
        if final_value != "A@" {
            failures.push(format!(
                "dom={dom}: only Shift+A and AltGr+Q should type; final value={final_value:?}; {log:?}"
            ));
        }
    }
    assert!(
        failures.is_empty(),
        "keyboard regression failures:\n{}",
        failures.join("\n")
    );
}
