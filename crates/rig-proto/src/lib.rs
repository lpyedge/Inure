//! RIG 共享协议定义。
//!
//! 这个 crate 是 AI 通道（Observation / Action）的**契约**，被 `rig-edge-browser`
//! 与 `rig-eval` 使用。修改这里的类型等于修改契约，必须同时更新
//! `docs/PERCEPTION.md` 或 `docs/EDGE_BROWSER.md`，并补充测试。
//!
//! 约定：
//! - 观察与动作消息一律是 JSON。
//! - 任何凭证都放在 HTTP Header（`Authorization: Bearer ...`），**禁止放在 URL query**。

pub mod observation;
