//! 运行记录与指标汇总。

use serde::{Deserialize, Serialize};

/// 观察方式。同一批任务在每种方式下各跑一遍，才能比较。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    /// 基线：原始截图直接交给多模态模型，由模型自己估坐标。**所有改进都要和它比。**
    ScreenshotBaseline,
    /// 只给元素树（文本）。
    Tree,
    /// 元素树 + 带编号叠加的截图。
    TreeImage,
    /// 纯像素兜底流水线（OCR/检测/视觉）。
    PixelsOnly,
    /// 对照基线：agent-browser 的 snapshot 输出（G1 v2 批准的市面同类标尺，见 docs/EVAL.md §5）。
    AbSnapshot,
}

/// 一次任务运行的记录。运行器按行输出 JSON（JSON Lines），本 crate 负责汇总。
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RunRecord {
    pub task_id: String,
    pub mode: Mode,
    pub model: String,
    pub success: bool,
    pub steps: u32,
    pub input_tokens: u64,
    pub output_tokens: u64,
    /// 每次观察的耗时（毫秒），每步一个。
    pub observe_latencies_ms: Vec<u64>,
    /// 点错（点到了非目标元素或无响应位置）的次数。
    pub wrong_clicks: u32,
    /// 因陈旧检查被拒绝的动作次数。
    pub stale_rejections: u32,
    /// 是否发现秘密值泄露（必须恒为 false）。
    pub leaked_secret: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Summary {
    pub mode: Mode,
    pub runs: usize,
    pub success_rate: f64,
    pub mean_input_tokens_per_step: f64,
    pub mean_wrong_clicks: f64,
    pub p50_observe_ms: Option<u64>,
    pub p95_observe_ms: Option<u64>,
    pub leaks: usize,
}

/// 最近秩百分位。`p` 取 0..=100。空输入返回 None。
pub fn percentile(sorted: &[u64], p: f64) -> Option<u64> {
    if sorted.is_empty() {
        return None;
    }
    let rank = ((p / 100.0) * sorted.len() as f64).ceil().max(1.0) as usize;
    Some(sorted[rank.min(sorted.len()) - 1])
}

/// 按模式分组汇总，结果按 `Mode` 排序。
pub fn summarize(records: &[RunRecord]) -> Vec<Summary> {
    let mut modes: Vec<Mode> = records.iter().map(|r| r.mode).collect();
    modes.sort();
    modes.dedup();

    modes
        .into_iter()
        .map(|mode| {
            let rs: Vec<&RunRecord> = records.iter().filter(|r| r.mode == mode).collect();
            let runs = rs.len();
            let total_steps: u64 = rs.iter().map(|r| r.steps as u64).sum();
            let total_in: u64 = rs.iter().map(|r| r.input_tokens).sum();
            let mut lat: Vec<u64> = rs
                .iter()
                .flat_map(|r| r.observe_latencies_ms.iter().copied())
                .collect();
            lat.sort_unstable();
            Summary {
                mode,
                runs,
                success_rate: rs.iter().filter(|r| r.success).count() as f64 / runs as f64,
                mean_input_tokens_per_step: if total_steps == 0 {
                    0.0
                } else {
                    total_in as f64 / total_steps as f64
                },
                mean_wrong_clicks: rs.iter().map(|r| r.wrong_clicks as f64).sum::<f64>()
                    / runs as f64,
                p50_observe_ms: percentile(&lat, 50.0),
                p95_observe_ms: percentile(&lat, 95.0),
                leaks: rs.iter().filter(|r| r.leaked_secret).count(),
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(mode: Mode, success: bool, steps: u32, tokens: u64, lat: &[u64]) -> RunRecord {
        RunRecord {
            task_id: "t".into(),
            mode,
            model: "m".into(),
            success,
            steps,
            input_tokens: tokens,
            output_tokens: 0,
            observe_latencies_ms: lat.to_vec(),
            wrong_clicks: 0,
            stale_rejections: 0,
            leaked_secret: false,
        }
    }

    #[test]
    fn percentile_nearest_rank() {
        let v = [10, 20, 30, 40, 50, 60, 70, 80, 90, 100];
        assert_eq!(percentile(&v, 50.0), Some(50));
        assert_eq!(percentile(&v, 95.0), Some(100));
        assert_eq!(percentile(&v, 0.0), Some(10));
        assert_eq!(percentile(&[], 50.0), None);
    }

    #[test]
    fn summary_groups_by_mode_and_computes_rates() {
        let rs = vec![
            rec(Mode::ScreenshotBaseline, true, 4, 4000, &[800, 900]),
            rec(Mode::ScreenshotBaseline, false, 6, 6000, &[1000]),
            rec(Mode::Tree, true, 2, 600, &[50]),
            rec(Mode::Tree, true, 3, 900, &[70, 90]),
        ];
        let s = summarize(&rs);
        assert_eq!(s.len(), 2);

        let base = &s[0];
        assert_eq!(base.mode, Mode::ScreenshotBaseline);
        assert_eq!(base.runs, 2);
        assert!((base.success_rate - 0.5).abs() < 1e-9);
        assert!((base.mean_input_tokens_per_step - 1000.0).abs() < 1e-9);

        let tree = &s[1];
        assert_eq!(tree.mode, Mode::Tree);
        assert!((tree.success_rate - 1.0).abs() < 1e-9);
        assert!((tree.mean_input_tokens_per_step - 300.0).abs() < 1e-9);
        assert_eq!(tree.p95_observe_ms, Some(90));
    }

    #[test]
    fn leaks_are_counted() {
        let mut r = rec(Mode::Tree, true, 1, 1, &[]);
        r.leaked_secret = true;
        assert_eq!(summarize(&[r])[0].leaks, 1);
    }

    #[test]
    fn run_record_json_lines_roundtrip() {
        let r = rec(Mode::TreeImage, true, 1, 1, &[5]);
        let line = serde_json::to_string(&r).unwrap();
        assert!(!line.contains('\n'));
        assert_eq!(serde_json::from_str::<RunRecord>(&line).unwrap(), r);
    }
}
