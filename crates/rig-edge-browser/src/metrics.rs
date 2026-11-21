//! 轻量延迟计量（整改第四期 F-09 / R-19 共用底座）。
//!
//! 每段一个环形样本窗（最近 256 个样本，微秒），snapshot 出
//! p50/p95/p99/max 与累计计数。进程级单例，无外部依赖；
//! /health 与 /ready 从这里读数。跨机延迟不比时钟：视频路径
//! 用「帧龄」（now − 帧合成时刻）表达，见 human.rs。

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::Duration;

const WINDOW: usize = 256;

/// 一段延迟的滑动窗口统计。
pub struct Latency {
    samples: Mutex<VecDeque<u64>>,
    total: AtomicU64,
    sum_us: AtomicU64,
}

impl Latency {
    pub const fn new() -> Self {
        Self {
            samples: Mutex::new(VecDeque::new()),
            total: AtomicU64::new(0),
            sum_us: AtomicU64::new(0),
        }
    }

    pub fn record(&self, d: Duration) {
        let us = d.as_micros() as u64;
        self.total.fetch_add(1, Ordering::Relaxed);
        self.sum_us.fetch_add(us, Ordering::Relaxed);
        let mut q = self.samples.lock().unwrap();
        if q.len() == WINDOW {
            q.pop_front();
        }
        q.push_back(us);
    }

    /// 累计（次数, 总微秒）：量测某段代码前后的差值用
    pub fn totals(&self) -> (u64, u64) {
        (
            self.total.load(Ordering::Relaxed),
            self.sum_us.load(Ordering::Relaxed),
        )
    }

    pub fn snapshot(&self) -> LatencySnapshot {
        let q = self.samples.lock().unwrap();
        let mut v: Vec<u64> = q.iter().copied().collect();
        drop(q);
        v.sort_unstable();
        let pick = |pct: usize| -> u64 {
            if v.is_empty() {
                0
            } else {
                v[((v.len() - 1) * pct) / 100]
            }
        };
        LatencySnapshot {
            count: self.total.load(Ordering::Relaxed),
            p50_us: pick(50),
            p95_us: pick(95),
            p99_us: pick(99),
            max_us: v.last().copied().unwrap_or(0),
        }
    }
}

#[derive(Clone, Copy, Default)]
pub struct LatencySnapshot {
    pub count: u64,
    pub p50_us: u64,
    pub p95_us: u64,
    pub p99_us: u64,
    pub max_us: u64,
}

impl LatencySnapshot {
    pub fn to_json(self) -> serde_json::Value {
        let ms = |us: u64| (us as f64) / 1000.0;
        serde_json::json!({
            "count": self.count,
            "p50_ms": ms(self.p50_us),
            "p95_ms": ms(self.p95_us),
            "p99_ms": ms(self.p99_us),
            "max_ms": ms(self.max_us),
        })
    }
}

/// 最近一次 observe 的量测（R-16：先量测再定向优化）。
#[derive(Clone, Default)]
pub struct LastObserve {
    pub total_ms: f64,
    pub cdp_calls: u64,
    pub cdp_ms: f64,
    pub quads_calls: u64,
    pub quads_ms: f64,
    pub ax_dom_ms: f64,
    pub nodes: usize,
}

pub struct Metrics {
    /// 人通道帧泵：合成段（解码+绘制）
    pub compose: Latency,
    /// 人通道帧泵：JPEG 编码段
    pub encode: Latency,
    /// AI act 全程（收到请求→结果）
    pub act: Latency,
    /// AI observe 全程
    pub observe: Latency,
    /// DOM 快照生成段（页内序列化往返 + 广播）
    pub dom_flush: Latency,
    /// 快照几何查询（DOM.getContentQuads 逐节点，R-16 量测点）
    pub quads: Latency,
    /// 帧泵排水丢弃的 screencast 帧数（只留最新）
    pub frames_skipped: AtomicU64,
    /// 已广播的帧数
    pub frames_sent: AtomicU64,
    /// 帧泵：整页帧数
    pub pump_full: AtomicU64,
    /// 帧泵：内容帧数
    pub pump_content: AtomicU64,
    /// 帧泵：内容帧成功贴图数
    pub pump_pasted: AtomicU64,
    /// 动作前单元素快速复核：确认跳过整页重采 / 退回整页快照的次数
    pub act_fast_hit: AtomicU64,
    pub act_fast_miss: AtomicU64,
    /// 已采集候选帧，但投影视图相同而输出空增量的次数
    pub observe_unchanged: AtomicU64,
    /// 帧泵：内容帧丢弃数（无唯一矩形/无干净底图/回退模式）
    pub pump_dropped: AtomicU64,
    /// 帧泵：rect 几何重取次数
    pub pump_rect_refetch: AtomicU64,
    pub last_observe: Mutex<Option<LastObserve>>,
}

impl Metrics {
    const fn new() -> Self {
        Self {
            compose: Latency::new(),
            encode: Latency::new(),
            act: Latency::new(),
            observe: Latency::new(),
            dom_flush: Latency::new(),
            quads: Latency::new(),
            frames_skipped: AtomicU64::new(0),
            frames_sent: AtomicU64::new(0),
            pump_full: AtomicU64::new(0),
            pump_content: AtomicU64::new(0),
            pump_pasted: AtomicU64::new(0),
            act_fast_hit: AtomicU64::new(0),
            act_fast_miss: AtomicU64::new(0),
            observe_unchanged: AtomicU64::new(0),
            pump_dropped: AtomicU64::new(0),
            pump_rect_refetch: AtomicU64::new(0),
            last_observe: Mutex::new(None),
        }
    }

    pub fn set_last_observe(&self, o: LastObserve) {
        *self.last_observe.lock().unwrap() = Some(o);
    }
}

static GLOBAL: Metrics = Metrics::new();

pub fn global() -> &'static Metrics {
    &GLOBAL
}
