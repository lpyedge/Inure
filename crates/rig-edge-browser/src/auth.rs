//! 直连鉴权：启动令牌换会话票据（docs/HARDENING_PLAN_2026-11-22.md §二）。
//!
//! - 启动令牌（`--token`，由启动程序随机生成）只用于 `login` 换票，
//!   或由非浏览器客户端直接作 Bearer 调用 AI 路由。
//! - 换得 access（短效）+ refresh（无时限，每次使用即轮换）。viewer
//!   在 access 将过期时用 refresh 静默续期，无登录时长。
//! - 票据只存内存：进程重启即全部失效，无需吊销接口。
//! - `stream_key`：HLS 的 `<video src>` 无法带请求头，媒体路径用这把
//!   只读、仅限 /stream 的随机钥匙；认证过的 viewer 才拿得到。

use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;
use std::time::{Duration, Instant};

const ACCESS_TTL: Duration = Duration::from_secs(15 * 60);

/// 同时有效的 refresh 票据上限（单人受信工具，几个标签页/设备足够）。
/// 超出时最早签发的那张作废，不改变“无时限、用一次轮换一次”的续期机制。
const MAX_ACTIVE_REFRESH: usize = 32;

/// 同时有效的 access 票据上限，避免长期运行堆积过多有效票据。
/// 超出时最早过期的那张先被挤掉。
const MAX_ACTIVE_ACCESS: usize = 64;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TokenPair {
    pub access: String,
    pub refresh: String,
    pub expires_in: u64,
}

pub struct Auth {
    master: String,
    stream_key: String,
    access_ttl: Duration,
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    access: HashMap<String, Instant>,
    /// 按签发先后排列，队首最早。
    refresh: VecDeque<String>,
}

fn random_token() -> String {
    let mut b = [0u8; 32];
    // 系统随机源不可用时无法安全运行，直接终止比发弱令牌好
    getrandom::fill(&mut b).expect("system random source unavailable");
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// 常量时间比较，避免按字节提前返回泄露令牌前缀
fn ct_eq(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    let mut diff = a.len() ^ b.len();
    for i in 0..a.len().max(b.len()) {
        diff |= usize::from(a.get(i).copied().unwrap_or(0) ^ b.get(i).copied().unwrap_or(0));
    }
    diff == 0
}

impl Auth {
    pub fn new(master: String) -> Self {
        Self::with_ttl(master, ACCESS_TTL)
    }

    fn with_ttl(master: String, access_ttl: Duration) -> Self {
        Auth {
            master,
            stream_key: random_token(),
            access_ttl,
            state: Mutex::new(State::default()),
        }
    }

    pub fn is_master(&self, t: &str) -> bool {
        ct_eq(&self.master, t)
    }

    pub fn stream_key(&self) -> &str {
        &self.stream_key
    }

    fn issue(&self, st: &mut State) -> TokenPair {
        let pair = TokenPair {
            access: random_token(),
            refresh: random_token(),
            expires_in: self.access_ttl.as_secs(),
        };
        let now = Instant::now();
        st.access.retain(|_, exp| *exp > now);
        // 达到上限时先挤掉最早过期的那张
        while st.access.len() >= MAX_ACTIVE_ACCESS {
            if let Some(oldest) = st
                .access
                .iter()
                .min_by_key(|(_, exp)| **exp)
                .map(|(k, _)| k.clone())
            {
                st.access.remove(&oldest);
            } else {
                break;
            }
        }
        st.access.insert(pair.access.clone(), now + self.access_ttl);
        while st.refresh.len() >= MAX_ACTIVE_REFRESH {
            st.refresh.pop_front();
        }
        st.refresh.push_back(pair.refresh.clone());
        pair
    }

    /// 用启动令牌换一对票据
    pub fn login(&self, token: &str) -> Option<TokenPair> {
        if !self.is_master(token) {
            return None;
        }
        let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        Some(self.issue(&mut st))
    }

    /// 用 refresh 换新一对；旧 refresh 立即作废（轮换），被盗用时
    /// 真假双方必有一方失效而可被察觉
    pub fn refresh(&self, refresh: &str) -> Option<TokenPair> {
        let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let pos = st.refresh.iter().position(|r| ct_eq(r, refresh))?;
        st.refresh.remove(pos);
        Some(self.issue(&mut st))
    }

    pub fn check_access(&self, access: &str) -> bool {
        let st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        st.access
            .get(access)
            .is_some_and(|exp| *exp > Instant::now())
    }

    pub fn check_stream_key(&self, k: &str) -> bool {
        ct_eq(&self.stream_key, k)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn login_requires_master_token() {
        let a = Auth::new("master".into());
        assert!(a.login("wrong").is_none());
        assert!(a.login("").is_none());
        let p = a.login("master").expect("正确令牌应换票");
        assert!(a.check_access(&p.access));
        assert!(!a.check_access("master"), "主令牌不是 access 票据");
        assert!(!a.check_access(&p.refresh), "refresh 不能当 access 用");
    }

    #[test]
    fn refresh_rotates_and_old_is_revoked() {
        let a = Auth::new("m".into());
        let p1 = a.login("m").unwrap();
        let p2 = a.refresh(&p1.refresh).expect("首次续期成功");
        assert_ne!(p1.refresh, p2.refresh);
        assert!(a.refresh(&p1.refresh).is_none(), "旧 refresh 不得重放");
        assert!(
            a.refresh(&p2.refresh).is_some(),
            "新 refresh 可继续续期（无时限）"
        );
        assert!(a.refresh("garbage").is_none());
    }

    #[test]
    fn active_refresh_tokens_are_bounded_and_oldest_is_evicted() {
        let a = Auth::new("m".into());
        let first = a.login("m").unwrap();
        let mut last = first.clone();
        for _ in 0..MAX_ACTIVE_REFRESH + 4 {
            last = a.login("m").unwrap();
        }
        assert!(
            a.state.lock().unwrap().refresh.len() <= MAX_ACTIVE_REFRESH,
            "活动 refresh 数量必须有上限"
        );
        assert!(a.refresh(&first.refresh).is_none(), "最早的 refresh 被挤掉");
        assert!(a.refresh(&last.refresh).is_some(), "最近的 refresh 仍有效");
    }

    #[test]
    fn active_access_tokens_are_bounded_and_soonest_expiring_is_evicted() {
        let a = Auth::with_ttl("m".into(), Duration::from_secs(3600));
        let first = a.login("m").unwrap();
        // 停一下让首张票据的过期时刻严格早于后续票据
        std::thread::sleep(Duration::from_millis(2));
        let mut last = first.clone();
        for _ in 0..MAX_ACTIVE_ACCESS + 4 {
            last = a.login("m").unwrap();
        }
        assert!(
            a.state.lock().unwrap().access.len() <= MAX_ACTIVE_ACCESS,
            "活动 access 数量必须有上限"
        );
        assert!(!a.check_access(&first.access), "最早过期的 access 被挤掉");
        assert!(a.check_access(&last.access), "最近的 access 仍有效");
    }

    #[test]
    fn access_expires() {
        let a = Auth::with_ttl("m".into(), Duration::from_millis(30));
        let p = a.login("m").unwrap();
        assert!(a.check_access(&p.access));
        std::thread::sleep(Duration::from_millis(60));
        assert!(!a.check_access(&p.access), "access 过期后失效");
        assert!(a.refresh(&p.refresh).is_some(), "过期不影响 refresh 续期");
    }

    #[test]
    fn restart_revokes_everything() {
        let a = Auth::new("m".into());
        let p = a.login("m").unwrap();
        let b = Auth::new("m".into());
        assert!(!b.check_access(&p.access), "新进程不认旧票据");
        assert!(b.refresh(&p.refresh).is_none());
        assert!(!b.check_stream_key(a.stream_key()));
    }

    #[test]
    fn ct_eq_basics() {
        assert!(ct_eq("abc", "abc"));
        assert!(!ct_eq("abc", "abd"));
        assert!(!ct_eq("abc", "abcd"));
        assert!(!ct_eq("", "a"));
    }
}
