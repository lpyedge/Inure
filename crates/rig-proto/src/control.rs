//! Connector 控制通道消息（`GET /v1/connector/control`，WebSocket Text 帧，JSON）。
//!
//! 握手顺序：
//! 1. Connector → `Hello`
//! 2. Gateway → `Challenge`（随机 nonce）
//! 3. Connector → `AuthResponse`（对 [`crate::auth::auth_payload`] 的 Ed25519 签名）
//! 4. Gateway → `Welcome`，或 `Error` 后关闭连接
//!
//! 之后 Gateway 通过 `OpenStream` 让 Connector 回拨数据 WSS（每条流一条 WSS）。
//! 心跳使用 WebSocket 原生 Ping/Pong，不定义应用层心跳消息。

use serde::{Deserialize, Serialize};

use crate::ids::{DeviceId, StreamId, UnixMillis};

/// Connector 发给 Gateway 的消息。
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum ConnectorMsg {
    Hello {
        proto_version: u32,
        device_id: DeviceId,
        connector_version: String,
        /// Connector 声明的能力，例如 "tcp"。Gateway 只能请求声明过的能力。
        capabilities: Vec<String>,
        /// Connector 本地策略里登记的目标（只有名字和协议，不含地址）。
        /// Gateway 用它做资源发现；地址只存在于 Connector 本地配置中。
        targets: Vec<AdvertisedTarget>,
    },
    AuthResponse {
        /// base64url（无填充）编码的 Ed25519 签名。
        signature: String,
    },
    /// Connector 拒绝打开某条流（例如本地策略不允许、目标连接失败）。
    /// 接受时不发消息，直接回拨数据 WSS。
    StreamRejected {
        stream_id: StreamId,
        code: ErrorCode,
        message: String,
    },
    /// （V2）确认已安装新的控制 Epoch，并已清理旧输入状态。
    EpochInstalled { domain: String, epoch: u64 },
}

/// Gateway 发给 Connector 的消息。
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum GatewayMsg {
    Challenge {
        /// base64url（无填充）编码的 32 字节随机数。
        nonce: String,
        /// Gateway 对外的 origin，例如 "https://rig.example.com"，会被纳入签名载荷，
        /// 防止签名被转发到另一个 Gateway 重放。
        gateway_origin: String,
    },
    Welcome {
        /// 建议的 WS Ping 间隔（秒）。
        heartbeat_secs: u32,
    },
    /// 请求 Connector 连接本地目标并回拨 `/v1/streams/{stream_id}/connector`。
    OpenStream {
        stream_id: StreamId,
        /// 一次性回拨凭证，放在回拨请求的 `Authorization: Bearer` 中。
        stream_token: String,
        /// Connector 本地目标的**名字**（见 `AdvertisedTarget::name`）。
        /// 刻意不传地址：Gateway 无法让 Connector 连接本地策略之外的任何地址。
        target: String,
        /// 超过该时间未回拨，Gateway 丢弃这条待配对的流。
        deadline: UnixMillis,
    },
    /// 立即关闭某条流（例如 Session 被撤销）。
    CloseStream {
        stream_id: StreamId,
        reason: String,
    },
    /// （V2）安装新的控制 Epoch，小于它的输入必须一律拒绝。
    InstallEpoch {
        domain: String,
        epoch: u64,
    },
    /// 设备已被撤销，Connector 应停止重连直到重新注册。
    Revoked {
        reason: String,
    },
    Error {
        code: ErrorCode,
        message: String,
    },
}

/// Connector 向 Gateway 公布的本地目标。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdvertisedTarget {
    /// 本地唯一名字，例如 "ssh"、"vnc"、"rdp"。
    pub name: String,
    /// 协议提示，例如 "ssh"、"rdp"、"vnc"、"tcp"。只用于展示和选择 Viewer，不影响转发。
    pub protocol: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    UnsupportedVersion,
    AuthFailed,
    DeviceRevoked,
    PolicyDenied,
    TargetUnreachable,
    Timeout,
    RateLimited,
    Internal,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tag_format_is_stable() {
        let m = ConnectorMsg::Hello {
            proto_version: 1,
            device_id: DeviceId("dev_1".into()),
            connector_version: "0.1.0".into(),
            capabilities: vec!["tcp".into()],
            targets: vec![AdvertisedTarget {
                name: "ssh".into(),
                protocol: "ssh".into(),
            }],
        };
        let s = serde_json::to_string(&m).unwrap();
        assert!(s.starts_with(r#"{"t":"hello""#), "{s}");
        let back: ConnectorMsg = serde_json::from_str(&s).unwrap();
        assert_eq!(back, m);
    }

    #[test]
    fn unknown_message_is_an_error_not_a_panic() {
        let r: Result<GatewayMsg, _> = serde_json::from_str(r#"{"t":"no_such_msg"}"#);
        assert!(r.is_err());
    }
}
