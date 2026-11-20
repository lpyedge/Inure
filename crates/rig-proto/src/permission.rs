//! 权限词表（封闭枚举）。
//!
//! 规则：**只能新增，不能修改已有项的含义，不能复用已删除的名字。**
//! 观察与控制分离；高权限能力（shell、exec、文件、完整 CDP、adb）必须单独授权，
//! 不会被 `control` 隐含。
//!
//! 与感知层的对应（见 docs/PERCEPTION.md）：
//! - `observe`：获取 Observation（语义快照）与视频，不能产生任何输入。
//! - `control`：执行 Action（点击、输入、滚动、导航）。还需要有效 Lease/Epoch。
//! - `cdp_full`：透明的完整 CDP 通道，绕过语义层与动作校验，必须单独授权。

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Permission {
    /// 建立原生 TCP 字节流（SSH/RDP/VNC 等由客户端自己说协议）。
    Connect,
    /// 观察画面/输出，不能输入。
    Observe,
    /// 向共享输入对象发送输入。还需要持有对应 ControlDomain 的有效 Lease。
    Control,
    /// 交互式 Shell（Web 终端 / Connector 端 PTY）。
    Shell,
    /// 非交互命令执行（SSH exec：stdout/stderr/退出码分离）。AI 的默认终端能力，仍属高权限。
    Exec,
    /// 文件上传/下载。
    File,
    /// 剪贴板读取。
    ClipboardRead,
    /// 完整、透明的 CDP 访问。高权限，应运行在隔离的浏览器 Profile。
    CdpFull,
    /// Android adb 访问。等同设备完全控制权。
    Adb,
}

impl Permission {
    /// 是否属于高权限能力（UI 上需要明确提示，审计中单独标注）。
    pub fn is_high_privilege(self) -> bool {
        matches!(
            self,
            Permission::Shell
                | Permission::Exec
                | Permission::File
                | Permission::CdpFull
                | Permission::Adb
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serde_names_are_stable() {
        // 这些字符串是对外契约，改动会破坏已签发的 Grant 和数据库中的数据。
        let cases = [
            (Permission::Connect, "\"connect\""),
            (Permission::Observe, "\"observe\""),
            (Permission::Control, "\"control\""),
            (Permission::Shell, "\"shell\""),
            (Permission::Exec, "\"exec\""),
            (Permission::File, "\"file\""),
            (Permission::ClipboardRead, "\"clipboard_read\""),
            (Permission::CdpFull, "\"cdp_full\""),
            (Permission::Adb, "\"adb\""),
        ];
        for (p, s) in cases {
            assert_eq!(serde_json::to_string(&p).unwrap(), s);
        }
    }
}
