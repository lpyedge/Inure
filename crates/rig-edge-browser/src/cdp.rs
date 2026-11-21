//! 极简 CDP 客户端：一条 WebSocket 连接，多路会话（flatten 模式）。
//!
//! 只实现边缘程序需要的子集：请求/响应按 id 关联，事件按 (sessionId, method)
//! 广播给订阅者。目标（含跨进程 iframe）通过 Target.setAutoAttach 全部挂载。

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::sync::{broadcast, mpsc, oneshot, Mutex};
use tokio_tungstenite::tungstenite::Message;

pub type CdpResult<T> = Result<T, String>;

/// 一条 CDP 事件。session 为 None 表示浏览器级事件。
#[derive(Clone, Debug)]
pub struct CdpEvent {
    pub session: Option<String>,
    pub method: String,
    pub params: Value,
}

struct Pending {
    tx: oneshot::Sender<CdpResult<Value>>,
}

/// 同步事件钩子：必须轻量、不阻塞（在读泵里直接调用）。
pub type EventTap = Arc<dyn Fn(&CdpEvent) + Send + Sync>;

pub struct Cdp {
    cmd_tx: mpsc::Sender<String>,
    pending: Arc<Mutex<HashMap<u64, Pending>>>,
    events: broadcast::Sender<CdpEvent>,
    /// 同步事件钩子：在读泵里、广播之前调用。同一连接上事件总先于其后
    /// 的命令响应被读到，钩子因此保证“先记账、后放行响应”的确定顺序
    /// （异步订阅者做不到：它可能晚于后续响应才被调度）。
    taps: Arc<std::sync::Mutex<Vec<EventTap>>>,
    next_id: AtomicU64,
    /// 读泵存活标志（R-19 可观测：/health 的 cdp_connected）
    alive: Arc<std::sync::atomic::AtomicBool>,
    /// 累计命令数与累计耗时微秒（R-16 量测：前后差值）
    calls: AtomicU64,
    call_us: AtomicU64,
}

impl Cdp {
    /// 连接到 Chromium 的浏览器级 WebSocket 端点。
    pub async fn connect(ws_url: &str) -> CdpResult<Arc<Cdp>> {
        let (ws, _) = tokio_tungstenite::connect_async(ws_url)
            .await
            .map_err(|e| format!("CDP 连接失败: {e}"))?;
        let (mut sink, mut stream) = ws.split();
        // 有界队列（GPT review R-08）：CDP 对端僵死时发送侧不能无限积压
        let (cmd_tx, mut cmd_rx) = mpsc::channel::<String>(128);
        let pending: Arc<Mutex<HashMap<u64, Pending>>> = Arc::new(Mutex::new(HashMap::new()));
        let (events, _) = broadcast::channel::<CdpEvent>(1024);

        // 写泵
        tokio::spawn(async move {
            while let Some(msg) = cmd_rx.recv().await {
                if sink.send(Message::Text(msg.into())).await.is_err() {
                    break;
                }
            }
        });
        // 读泵
        let pending2 = pending.clone();
        let events2 = events.clone();
        let taps: Arc<std::sync::Mutex<Vec<EventTap>>> =
            Arc::new(std::sync::Mutex::new(Vec::new()));
        let taps2 = taps.clone();
        let alive = Arc::new(std::sync::atomic::AtomicBool::new(true));
        let alive2 = alive.clone();
        tokio::spawn(async move {
            while let Some(Ok(msg)) = stream.next().await {
                let Message::Text(text) = msg else { continue };
                let Ok(v) = serde_json::from_str::<Value>(&text) else {
                    continue;
                };
                if let Some(id) = v.get("id").and_then(Value::as_u64) {
                    let res = if let Some(err) = v.get("error") {
                        Err(format!("CDP 错误: {err}"))
                    } else {
                        Ok(v.get("result").cloned().unwrap_or(Value::Null))
                    };
                    if let Some(p) = pending2.lock().await.remove(&id) {
                        let _ = p.tx.send(res);
                    }
                } else if let Some(method) = v.get("method").and_then(Value::as_str) {
                    let ev = CdpEvent {
                        session: v
                            .get("sessionId")
                            .and_then(Value::as_str)
                            .map(str::to_string),
                        method: method.to_string(),
                        params: v.get("params").cloned().unwrap_or(Value::Null),
                    };
                    for t in taps2.lock().unwrap().iter() {
                        t(&ev);
                    }
                    let _ = events2.send(ev);
                }
            }
            // 读泵结束 = 连接已断：让全部在途命令立即失败，
            // 不必各自等满超时（R-13）
            alive2.store(false, std::sync::atomic::Ordering::SeqCst);
            let mut map = pending2.lock().await;
            for (_, p) in map.drain() {
                let _ = p.tx.send(Err("CDP 连接断开".to_string()));
            }
        });

        Ok(Arc::new(Cdp {
            cmd_tx,
            pending,
            events,
            taps,
            next_id: AtomicU64::new(1),
            alive,
            calls: AtomicU64::new(0),
            call_us: AtomicU64::new(0),
        }))
    }

    /// CDP 连接是否还活着（读泵未退出）
    pub fn is_alive(&self) -> bool {
        self.alive.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// 累计（命令数, 耗时微秒），量测时取前后差值
    pub fn call_stats(&self) -> (u64, u64) {
        (
            self.calls.load(std::sync::atomic::Ordering::Relaxed),
            self.call_us.load(std::sync::atomic::Ordering::Relaxed),
        )
    }

    /// 注册同步事件钩子（见 `taps`）。
    pub fn tap(&self, f: EventTap) {
        self.taps.lock().unwrap().push(f);
    }

    pub fn subscribe(&self) -> broadcast::Receiver<CdpEvent> {
        self.events.subscribe()
    }

    /// 发送一条命令。session 为 None 时是浏览器级命令。
    pub async fn call(
        &self,
        session: Option<&str>,
        method: &str,
        params: Value,
    ) -> CdpResult<Value> {
        let started = std::time::Instant::now();
        let res = self.call_inner(session, method, params).await;
        // R-16 量测：累计命令数与耗时（成功失败都算）
        self.calls
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.call_us.fetch_add(
            started.elapsed().as_micros() as u64,
            std::sync::atomic::Ordering::Relaxed,
        );
        res
    }

    async fn call_inner(
        &self,
        session: Option<&str>,
        method: &str,
        params: Value,
    ) -> CdpResult<Value> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = oneshot::channel();
        {
            let mut map = self.pending.lock().await;
            // 在途上限（R-13）：对端僵死时不让 pending 无界增长
            if map.len() >= 512 {
                return Err(format!("CDP 在途命令过多（{method}）"));
            }
            map.insert(id, Pending { tx });
        }
        let mut msg = json!({"id": id, "method": method, "params": params});
        if let Some(s) = session {
            msg["sessionId"] = json!(s);
        }
        // 总截止（R-13）：排队发送与等响应共用同一个 20 秒——
        // 此前 send 不在超时保护内，队列满 + 写泵堵时可无限挂。
        // 注意：超时后消息仍可能已被对端执行（结果不确定），
        // 调用方对副作用命令不得盲重试（与 R-06 口径一致）
        let send_and_wait = async {
            if self.cmd_tx.send(msg.to_string()).await.is_err() {
                // 发送失败也要清 pending，否则条目永久滞留（R-08）
                self.pending.lock().await.remove(&id);
                return Err("CDP 写通道已关闭".to_string());
            }
            match rx.await {
                Ok(res) => res,
                Err(_) => {
                    self.pending.lock().await.remove(&id);
                    Err(format!("CDP 通道关闭（{method}）"))
                }
            }
        };
        match tokio::time::timeout(std::time::Duration::from_secs(20), send_and_wait).await {
            Ok(res) => res,
            Err(_) => {
                // 超时后应答若再到，读泵 remove 时已无条目、无副作用；
                // 这里先清，避免在途条目无限累积（R-08）
                self.pending.lock().await.remove(&id);
                Err(format!("CDP 调用超时（{method}）"))
            }
        }
    }
}
