//! RIG 共享协议定义。
//!
//! 这个 crate 是 AI 通道（Observation / Action）的**契约**，被 `rig-edge-browser`
//! 与 `rig-eval` 使用。修改这里的类型等于修改契约，必须同时更新
//! `docs/PERCEPTION.md` 或 `docs/EDGE_BROWSER.md`，并补充测试。
//!
//! 约定：
//! - 观察与动作消息一律是 JSON，字段 `t` 标识类型。
//! - 任何凭证都放在 HTTP Header（`Authorization: Bearer ...`），**禁止放在 URL query**。
//!
//! 历史注记：网关/传输层（Gateway/Connector/CLI）已整仓移除；`auth`、`control`、
//! `permission`、`stream`、`ids` 模块是当时的传输契约残留，仅保留不断裂编译，
//! 新代码不得新增对它们的依赖。

pub mod auth;
pub mod control;
pub mod ids;
pub mod observation;
pub mod permission;
pub mod stream;

/// 协议版本。Connector 在 `Hello` 中声明，Gateway 不支持时必须拒绝并返回明确错误。
pub const PROTOCOL_VERSION: u32 = 1;

/// 单租户部署时使用的默认租户。所有表都带 tenant_id，以后启用多租户无需迁移结构。
pub const DEFAULT_TENANT: &str = "default";
