//! 数据流约定（`/v1/streams/...`）。
//!
//! - WebSocket **Binary** 帧：原始字节，原样转发，不做任何编码。
//! - WebSocket **Text** 帧：只能是 [`StreamSignal`] 的 JSON。
//! - 半关闭：一端读到 EOF 时发送 `{"t":"eof"}`，对端收到后对目标 socket 执行
//!   `shutdown(Write)`，但继续读取另一方向的数据，直到双方都 EOF 才关闭 WebSocket。

use serde::{Deserialize, Serialize};

use crate::control::ErrorCode;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum StreamSignal {
    /// 发送方向已结束（TCP 半关闭）。
    Eof,
    /// 异常终止。
    Error { code: ErrorCode, message: String },
}

/// 单条流在 Gateway 每个方向上允许缓存的最大字节数（默认值，可配置）。
/// 超过后停止从另一端读取，把背压传回源头。
pub const DEFAULT_STREAM_BUFFER_BYTES: usize = 256 * 1024;

/// Gateway 等待 Connector 回拨数据 WSS 的默认超时。
pub const DEFAULT_RENDEZVOUS_TIMEOUT_MS: i64 = 10_000;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn eof_wire_format() {
        assert_eq!(
            serde_json::to_string(&StreamSignal::Eof).unwrap(),
            r#"{"t":"eof"}"#
        );
    }
}
