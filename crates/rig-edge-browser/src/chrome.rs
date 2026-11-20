//! 浏览器 chrome 状态（第五期起）：工具栏已改观看页原生 DOM
//! 呈现（viewer.html/viewer.js），本模块只保留**状态**——标签
//! 列表、导航可用性、加载态、页内寻找会话、分辨率模式与通道
//! 开关显示态。边缘经 WS `chrome-state` 通知把状态推给观看
//! 端渲染；观看端以 `chrome` 命令消息回送操作（见 human.rs
//! 的 chrome_command）。旧版把工具栏绘进帧像素的合成器、
//! 几何命中与字体绘制已整删（历史见 git 与 EDGE_BROWSER
//! §9.1 第五期条目）。

use serde_json::{json, Value};

#[derive(Clone, Debug)]
pub struct TabView {
    pub target_id: String,
    pub title: String,
    pub url: String,
}

#[derive(Clone, Debug, Default)]
pub struct FindState {
    pub text: String,
    /// 如 "3 处"；空串 = 无结果或未搜
    pub info: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ResMode {
    Auto,
    Fixed(u32, u32),
}

#[derive(Clone, Debug)]
pub struct ChromeState {
    pub tabs: Vec<TabView>,
    pub active: usize,
    pub can_back: bool,
    pub can_fwd: bool,
    pub loading: bool,
    pub find: Option<FindState>,
    pub res: ResMode,
    /// 菜单勾选态显示用（真实开关在 main.rs 的 gate）
    pub gate_ai: bool,
    pub gate_human: bool,
}

impl Default for ChromeState {
    fn default() -> Self {
        ChromeState {
            tabs: Vec::new(),
            active: 0,
            can_back: false,
            can_fwd: false,
            loading: false,
            find: None,
            res: ResMode::Auto,
            gate_ai: true,
            gate_human: true,
        }
    }
}

impl ChromeState {
    /// 推给观看端的 chrome-state 通知体（观看端据此渲染原生
    /// 工具栏：标签条/导航按钮/地址栏/寻找条/菜单勾选态）。
    pub fn state_json(&self) -> Value {
        json!({
            "type": "chrome-state",
            "tabs": self.tabs.iter().map(|t| json!({
                "title": t.title,
                "url": t.url,
            })).collect::<Vec<_>>(),
            "active": self.active,
            "canBack": self.can_back,
            "canFwd": self.can_fwd,
            "loading": self.loading,
            "find": self.find.as_ref().map(|f| json!({
                "text": f.text,
                "info": f.info,
            })),
            "res": match self.res {
                ResMode::Auto => "auto".to_string(),
                ResMode::Fixed(w, h) => format!("{w}x{h}"),
            },
            "gateAi": self.gate_ai,
            "gateHuman": self.gate_human,
        })
    }
}

/// 地址栏输入归一化：无协议头的按域名/搜索处理
pub fn normalize_input(raw: &str) -> String {
    let t = raw.trim();
    if t.starts_with("http://")
        || t.starts_with("https://")
        || t.starts_with("chrome://")
        || t.starts_with("about:")
        || t.starts_with("file://")
    {
        return t.to_string();
    }
    let looks_like_host = !t.contains(char::is_whitespace)
        && (t.contains('.') || t.starts_with("localhost") || t.contains(':'));
    if looks_like_host {
        format!("https://{t}")
    } else {
        let mut q = String::new();
        for b in t.bytes() {
            match b {
                b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                    q.push(b as char)
                }
                _ => q.push_str(&format!("%{b:02X}")),
            }
        }
        format!("https://www.google.com/search?q={q}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_input_cases() {
        assert_eq!(normalize_input("example.com"), "https://example.com");
        assert_eq!(normalize_input("http://a.b/c"), "http://a.b/c");
        assert_eq!(normalize_input("chrome://settings"), "chrome://settings");
        assert!(normalize_input("hello world").contains("google.com/search?q="));
        assert!(normalize_input("localhost:8080/x").starts_with("https://"));
    }

    #[test]
    fn state_json_shape() {
        let st = ChromeState {
            tabs: vec![TabView {
                target_id: "t1".into(),
                title: "标题".into(),
                url: "https://x.example/".into(),
            }],
            loading: true,
            ..Default::default()
        };
        let v = st.state_json();
        assert_eq!(v["type"], "chrome-state");
        assert_eq!(v["tabs"][0]["title"], "标题");
        assert_eq!(v["tabs"][0]["url"], "https://x.example/");
        // target_id 是边缘内部句柄，不外泄给观看端
        assert!(v["tabs"][0].get("target_id").is_none());
        assert_eq!(v["loading"], true);
        assert_eq!(v["res"], "auto");
        assert!(v["find"].is_null());
    }
}
