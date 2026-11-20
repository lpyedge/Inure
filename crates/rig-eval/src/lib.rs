//! 感知层评估框架。方法论见 `docs/EVAL.md`。
//!
//! 这个 crate 只放**与模型和浏览器无关**的部分：任务格式、运行记录、指标汇总、泄密检测。
//! 真正驱动浏览器和模型的运行器在 P1/P2 实现（可以先用 Python/TS 写在 `eval/` 下的独立脚本，
//! 只要它输出本 crate 定义的 `RunRecord`（JSON Lines）即可复用这里的汇总和报告）。

pub mod leak;
pub mod metrics;
pub mod task;
