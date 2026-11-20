//! ID 约定。
//!
//! - 所有 ID 都是字符串（数据库里存 TEXT），带类型前缀，便于日志排查且防止混用：
//!   `dev_` 设备/Connector，`res_` 资源，`tgt_` 目标，`grt_` Grant，`ses_` Session，
//!   `str_` Stream，`lse_` Lease，`evt_` 审计事件。
//! - 前缀后面的部分推荐 UUIDv7（时间有序），由 Gateway 生成。
//! - 时间一律使用 Unix 毫秒（i64），时钟以 Gateway 为准。

use serde::{Deserialize, Serialize};

macro_rules! id_type {
    ($name:ident, $prefix:literal) => {
        #[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(pub String);

        impl $name {
            pub const PREFIX: &'static str = $prefix;

            /// 校验前缀是否正确（不校验后半部分格式）。
            pub fn is_well_formed(&self) -> bool {
                self.0.starts_with(Self::PREFIX) && self.0.len() > Self::PREFIX.len()
            }
        }

        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(&self.0)
            }
        }
    };
}

id_type!(DeviceId, "dev_");
id_type!(ResourceId, "res_");
id_type!(TargetId, "tgt_");
id_type!(GrantId, "grt_");
id_type!(SessionId, "ses_");
id_type!(StreamId, "str_");
id_type!(LeaseId, "lse_");

/// Unix 毫秒时间戳。
pub type UnixMillis = i64;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefix_check() {
        assert!(DeviceId("dev_0191".into()).is_well_formed());
        assert!(!DeviceId("res_0191".into()).is_well_formed());
        assert!(!DeviceId("dev_".into()).is_well_formed());
    }
}
