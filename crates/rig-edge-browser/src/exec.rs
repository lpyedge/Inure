//! 人机交互模型的执行状态（docs/EDGE_BROWSER.md §7.1–7.3）。
//!
//! AI 通道与人间通道共享同一份状态：
//! - Idle（待命）：AI 已接入但不做识别处理；人自由操作。
//! - Executing（执行）：用户已下达具体指令、AI 正在执行；观看端
//!   遮蔽人的输入并显示 AI 指针；人可随时强制接管。
//!
//! 本模块只管状态与 AI 活动记录（指针位置/轨迹/意图标签），
//! 遮蔽与接管的动作分发在 human.rs，中断检查在 ai.rs。

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use tokio::sync::Mutex;

/// AI 指针可视化数据（§7.2）：第五期起由观看端 overlay 呈现，
/// 边缘只经 WS `ghost` 通知下发数据，不再绘进帧像素。
pub struct PointerVis {
    /// 页面坐标
    pub x: f32,
    pub y: f32,
    pub label: String,
    /// 轨迹点（页面坐标，旧→新）+ 各点不透明度 0..1
    pub trail: Vec<(f32, f32, f32)>,
    /// 点击波纹进度 0..1（None = 无）
    pub pulse: Option<f32>,
}

/// 执行结束后回待命的保持窗：覆盖「一条指令拆成多个 call」的间隔
const HOLDOVER: Duration = Duration::from_millis(2000);
/// 轨迹点保留时长
const TRAIL_TTL: Duration = Duration::from_millis(1200);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExecState {
    Idle,
    Executing,
}

struct Inner {
    state: ExecState,
    /// 代次：begin/request_abort 都递增；保持窗任务只在代次未变时回待命
    generation: u64,
    /// 在途 AI call 数（GPT review R-02）：保持窗只由最后一个
    /// 结束的 call 触发，单个 call 先结束不得提前回待命
    active: usize,
    /// 取消代次（GPT review R-01）：request_abort 递增；每个 call
    /// 在 begin 时领取当时的代次，代次不符即已被接管取消。新 call
    /// 领取新代次，永不复活旧 call 的取消状态
    cancel_epoch: u64,
    /// 接管闩锁（LIU 口径 2026-11-20）：接管后 act 一律拒绝，
    /// 直到一次 observe 成功完成（重新同步）才放行
    latched: bool,
    x: f32,
    y: f32,
    label: String,
    has_pos: bool,
    trail: VecDeque<(f32, f32, Instant)>,
    pulse_at: Option<Instant>,
}

/// 一个 AI call 的执行句柄：携带它领取的取消代次
#[derive(Clone, Copy, Debug)]
pub struct ExecHandle {
    epoch: u64,
}

pub struct ExecControl {
    inner: Mutex<Inner>,
    holdover: Duration,
}

impl Default for ExecControl {
    fn default() -> Self {
        Self::with_holdover(HOLDOVER)
    }
}

impl ExecControl {
    pub fn with_holdover(holdover: Duration) -> Self {
        ExecControl {
            inner: Mutex::new(Inner {
                state: ExecState::Idle,
                generation: 0,
                active: 0,
                cancel_epoch: 0,
                latched: false,
                x: 0.0,
                y: 0.0,
                label: String::new(),
                has_pos: false,
                trail: VecDeque::new(),
                pulse_at: None,
            }),
            holdover,
        }
    }

    async fn begin_inner(&self) -> ExecHandle {
        let mut g = self.inner.lock().await;
        g.state = ExecState::Executing;
        g.generation += 1;
        g.active += 1;
        ExecHandle {
            epoch: g.cancel_epoch,
        }
    }

    /// observe 开始：永不被闩锁拦（它是接管后的恢复路径）
    pub async fn begin_observe(self: &std::sync::Arc<Self>) -> ExecHandle {
        self.begin_inner().await
    }

    /// act 开始：接管闩锁未解时返回 None（调用方直接回 Aborted）
    pub async fn begin_act(self: &std::sync::Arc<Self>) -> Option<ExecHandle> {
        if self.inner.lock().await.latched {
            return None;
        }
        Some(self.begin_inner().await)
    }

    /// 一个 AI call 结束：在途计数归零时才启动保持窗回待命
    pub async fn end(self: &std::sync::Arc<Self>, _h: &ExecHandle) {
        let gen = {
            let mut g = self.inner.lock().await;
            g.active = g.active.saturating_sub(1);
            if g.active > 0 {
                return;
            }
            g.generation
        };
        let me = self.clone();
        tokio::spawn(async move {
            tokio::time::sleep(me.holdover).await;
            let mut g = me.inner.lock().await;
            if g.generation == gen && g.active == 0 && g.state == ExecState::Executing {
                g.state = ExecState::Idle;
                g.has_pos = false;
                g.trail.clear();
                g.pulse_at = None;
            }
        });
    }

    /// 一次 observe 成功完成：解除接管闩锁
    pub async fn note_observed(&self) {
        self.inner.lock().await.latched = false;
    }

    /// 强制接管：立即回待命、递增取消代次（在途 call 全部作废）、
    /// 置闩锁（后续 act 拒绝，直到 observe 重新同步）
    pub async fn request_abort(&self) {
        let mut g = self.inner.lock().await;
        g.state = ExecState::Idle;
        g.generation += 1;
        g.cancel_epoch += 1;
        g.latched = true;
        g.has_pos = false;
        g.trail.clear();
        g.pulse_at = None;
    }

    /// 当前在途 call 数（R-19 可观测：/health 的 exec_in_flight）
    pub async fn in_flight(&self) -> usize {
        self.inner.lock().await.active
    }

    pub async fn state(&self) -> ExecState {
        self.inner.lock().await.state
    }

    pub async fn is_executing(&self) -> bool {
        self.state().await == ExecState::Executing
    }

    /// 该句柄的 call 是否已被接管取消（代次比对，单向不可逆）
    pub async fn aborted(&self, h: &ExecHandle) -> bool {
        self.inner.lock().await.cancel_epoch != h.epoch
    }

    /// 记录一次 AI 动作的落点（页面坐标）与意图标签
    pub async fn record(&self, x: f32, y: f32, label: &str, pulse: bool) {
        let mut g = self.inner.lock().await;
        let now = Instant::now();
        if !g.has_pos || g.x != x || g.y != y {
            g.trail.push_back((x, y, now));
        }
        while let Some((_, _, t)) = g.trail.front() {
            if now.duration_since(*t) > TRAIL_TTL {
                g.trail.pop_front();
            } else {
                break;
            }
        }
        g.x = x;
        g.y = y;
        g.has_pos = true;
        if !label.is_empty() {
            g.label = label.to_string();
        }
        if pulse {
            g.pulse_at = Some(now);
        }
    }

    /// 组装当前要绘制的 AI 指针（待命期或尚无落点时为 None）
    pub async fn pointer_vis(&self) -> Option<PointerVis> {
        let g = self.inner.lock().await;
        if g.state != ExecState::Executing || !g.has_pos {
            return None;
        }
        let now = Instant::now();
        let trail = g
            .trail
            .iter()
            .map(|(x, y, t)| {
                let age = now.duration_since(*t).as_secs_f32();
                let a = (1.0 - age / TRAIL_TTL.as_secs_f32()).clamp(0.0, 1.0);
                (*x, *y, a)
            })
            .collect();
        let pulse = g.pulse_at.and_then(|t| {
            let age = now.duration_since(t).as_secs_f32();
            if age < 0.5 {
                Some(age / 0.5)
            } else {
                None
            }
        });
        Some(PointerVis {
            x: g.x,
            y: g.y,
            label: g.label.clone(),
            trail,
            pulse,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn ctl() -> Arc<ExecControl> {
        Arc::new(ExecControl::with_holdover(Duration::from_millis(60)))
    }

    /// R-02 回归：A、B 先后 begin，A 先 end，保持窗过后 B 仍在途，
    /// 状态必须保持 Executing；B end 后才回 Idle
    #[tokio::test]
    async fn holdover_waits_for_last_call() {
        let c = ctl();
        let a = c.begin_observe().await;
        let b = c.begin_act().await.expect("未闩锁应放行");
        c.end(&a).await;
        tokio::time::sleep(Duration::from_millis(150)).await;
        assert_eq!(
            c.state().await,
            ExecState::Executing,
            "B 仍在途时不得提前回待命"
        );
        c.end(&b).await;
        tokio::time::sleep(Duration::from_millis(150)).await;
        assert_eq!(c.state().await, ExecState::Idle, "全部结束后应回待命");
    }

    /// R-01 回归：接管后新 call 的 begin 不得复活旧 call——旧句柄
    /// 仍报已取消，新句柄正常
    #[tokio::test]
    async fn abort_is_not_revived_by_new_begin() {
        let c = ctl();
        let a = c.begin_act().await.expect("未闩锁应放行");
        c.request_abort().await;
        assert!(c.aborted(&a).await, "接管后在途 call 应已取消");
        let b = c.begin_observe().await;
        assert!(c.aborted(&a).await, "新 observe 不得复活旧 call 的取消");
        assert!(!c.aborted(&b).await, "新 call 本身未被取消");
    }

    /// 闩锁：接管后 act 被拒；observe 成功（note_observed）后放行
    #[tokio::test]
    async fn latch_blocks_act_until_observe() {
        let c = ctl();
        c.request_abort().await;
        assert!(c.begin_act().await.is_none(), "闩锁中 act 应被拒");
        let o = c.begin_observe().await;
        c.note_observed().await;
        c.end(&o).await;
        assert!(c.begin_act().await.is_some(), "observe 后 act 应放行");
    }
}
