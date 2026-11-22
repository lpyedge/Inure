//! 感知层评估框架。方法论见 `docs/EVAL.md`。
//!
//! 这个 crate 提供评估任务加载与模型目标校验、运行记录和指标汇总、泄密检测；不包含模型调用
//! 或浏览器驱动运行器。仓库中的 `eval/verify_fixtures.py` 使用真实 Chromium 校验任务判据、
//! 标准答案和陷阱路径，不运行模型。外部运行器可输出本 crate 定义的 `RunRecord`（JSON Lines），
//! 再复用这里的汇总功能。

pub mod leak;
pub mod metrics;
pub mod task;
