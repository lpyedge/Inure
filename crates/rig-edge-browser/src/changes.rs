//! 页面变化记录：把浏览器的结构类通知（DOM/Page 事件）归成一条有序、
//! 有界的变化日志，供“呈现”一侧判断页面变没变、变了多少——
//! 与“操作”（act）解耦，人/AI/页面自己引起的变化一视同仁。
//!
//! 实测依据（真 Chromium，探针 9 场景）：文字/属性/插入/删除/整页跳转/
//! pushState 都有通知且能定位节点；**滚动与纯视觉变化（CSS 动画、
//! :hover）没有任何通知**，所以“没收到通知”只代表 DOM 结构与属性
//! 没变，不代表页面看起来没变。需要位置的消费者必须自己现查几何。
//! `Accessibility.nodesUpdated` 在所有条件下都未触发，不使用。
//!
//! 保守原则：日志环形有界；消费者的基线序号已被淘汰、变化节点
//! 过多都按“整页变了”处理——宁可多采，不漏变。事件由 CDP 读泵
//! 同步写入（`Cdp::tap`），没有异步队列，不存在溢出丢事件。

use std::collections::VecDeque;
use std::sync::Mutex;

use serde_json::Value;

/// 单条事件的影响范围。
#[derive(Clone, Debug, PartialEq)]
pub enum Hit {
    /// 换文档/换页/路由变化：之前的一切定位都不可信，需整页重采。
    Document,
    /// 若干节点（CDP nodeId，会话内）的子树或自身变了。
    Nodes(Vec<i64>),
}

/// 把一条 CDP 事件归类；与页面状态无关的事件返回 None。
pub fn classify(method: &str, params: &Value) -> Option<Hit> {
    let id = |k: &str| params.get(k).and_then(Value::as_i64);
    match method {
        "DOM.documentUpdated" | "Page.navigatedWithinDocument" => Some(Hit::Document),
        "Page.frameNavigated" => Some(Hit::Document),
        "DOM.attributeModified"
        | "DOM.attributeRemoved"
        | "DOM.characterDataModified"
        | "DOM.childNodeCountUpdated" => id("nodeId").map(|n| Hit::Nodes(vec![n])),
        // 子节点增删：受影响的是父节点
        "DOM.childNodeInserted" | "DOM.childNodeRemoved" | "DOM.setChildNodes" => {
            id("parentNodeId").map(|n| Hit::Nodes(vec![n]))
        }
        "DOM.inlineStyleInvalidated" => {
            let ids: Vec<i64> = params
                .get("nodeIds")
                .and_then(Value::as_array)
                .map(|a| a.iter().filter_map(Value::as_i64).collect())
                .unwrap_or_default();
            if ids.is_empty() {
                None
            } else {
                Some(Hit::Nodes(ids))
            }
        }
        _ => None,
    }
}

/// 自某序号以来的变化汇总。
#[derive(Clone, Debug, PartialEq)]
pub enum Change {
    /// 自基线以来没有结构类通知。
    None,
    /// 只有这些 (会话, nodeId) 变了；数量已在上限内。
    Nodes(Vec<(String, i64)>),
    /// 换文档，或基线已被淘汰/通道溢出/变化太多：按整页处理。
    Document,
}

struct Entry {
    seq: u64,
    session: String,
    hit: Hit,
}

struct Inner {
    seq: u64,
    log: VecDeque<Entry>,
    /// 日志里最早一条的序号 - 1：基线小于它说明已被淘汰
    floor: u64,
}

pub struct ChangeLog {
    inner: Mutex<Inner>,
    cap: usize,
    /// 局部变化的节点数上限：超过就当整页变了（补算比整页采集还贵时不划算）
    max_nodes: usize,
}

impl ChangeLog {
    pub fn new(cap: usize, max_nodes: usize) -> Self {
        Self {
            inner: Mutex::new(Inner {
                seq: 0,
                log: VecDeque::new(),
                floor: 0,
            }),
            cap: cap.max(1),
            max_nodes,
        }
    }

    /// 当前序号：消费者在开始采集**之前**取，采集期间到达的事件算“之后”
    /// （保守，不会漏）。
    pub fn mark(&self) -> u64 {
        self.inner.lock().unwrap().seq
    }

    pub fn record(&self, session: &str, hit: Hit) {
        let mut g = self.inner.lock().unwrap();
        g.seq += 1;
        let seq = g.seq;
        g.log.push_back(Entry {
            seq,
            session: session.to_string(),
            hit,
        });
        while g.log.len() > self.cap {
            if let Some(e) = g.log.pop_front() {
                g.floor = e.seq;
            }
        }
    }

    pub fn since(&self, mark: u64) -> Change {
        let g = self.inner.lock().unwrap();
        if mark < g.floor {
            return Change::Document;
        }
        let mut nodes: Vec<(String, i64)> = Vec::new();
        for e in g.log.iter().filter(|e| e.seq > mark) {
            match &e.hit {
                Hit::Document => return Change::Document,
                Hit::Nodes(ids) => {
                    for n in ids {
                        let key = (e.session.clone(), *n);
                        if !nodes.contains(&key) {
                            nodes.push(key);
                            if nodes.len() > self.max_nodes {
                                return Change::Document;
                            }
                        }
                    }
                }
            }
        }
        if nodes.is_empty() {
            Change::None
        } else {
            Change::Nodes(nodes)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn classify_follows_probe_findings() {
        // 探针 B：文字变化 = 删旧文本节点 + 插新文本节点，受影响的是父节点
        assert_eq!(
            classify(
                "DOM.childNodeRemoved",
                &json!({"parentNodeId": 7, "nodeId": 9})
            ),
            Some(Hit::Nodes(vec![7]))
        );
        assert_eq!(
            classify(
                "DOM.childNodeInserted",
                &json!({"parentNodeId": 7, "node": {}})
            ),
            Some(Hit::Nodes(vec![7]))
        );
        // 探针 C/E2：属性变化定位到节点本身
        assert_eq!(
            classify(
                "DOM.attributeModified",
                &json!({"nodeId": 12, "name": "disabled"})
            ),
            Some(Hit::Nodes(vec![12]))
        );
        // 探针 E：内联样式失效带 nodeIds 数组
        assert_eq!(
            classify("DOM.inlineStyleInvalidated", &json!({"nodeIds": [22, 23]})),
            Some(Hit::Nodes(vec![22, 23]))
        );
        // 探针 F/G：换页、路由变化 = 整页
        assert_eq!(
            classify("DOM.documentUpdated", &json!({})),
            Some(Hit::Document)
        );
        assert_eq!(
            classify("Page.frameNavigated", &json!({"frame": {"id": "F"}})),
            Some(Hit::Document)
        );
        assert_eq!(
            classify("Page.navigatedWithinDocument", &json!({"url": "/r2"})),
            Some(Hit::Document)
        );
        // 无关事件不记（lifecycle 是开启时的补发，不是变化信号）
        assert_eq!(classify("Page.lifecycleEvent", &json!({})), None);
        assert_eq!(classify("Accessibility.nodesUpdated", &json!({})), None);
        // 缺字段不臆造
        assert_eq!(classify("DOM.attributeModified", &json!({})), None);
    }

    #[test]
    fn nothing_recorded_means_none() {
        let log = ChangeLog::new(16, 8);
        let m = log.mark();
        assert_eq!(log.since(m), Change::None);
    }

    #[test]
    fn local_changes_are_deduped_and_scoped_to_baseline() {
        let log = ChangeLog::new(16, 8);
        log.record("s1", Hit::Nodes(vec![5]));
        let m = log.mark(); // 基线在第一条之后
        log.record("s1", Hit::Nodes(vec![7]));
        log.record("s1", Hit::Nodes(vec![7])); // 同节点重复只算一次
        log.record("s2", Hit::Nodes(vec![7])); // 不同会话同 nodeId 不是同一个
        assert_eq!(
            log.since(m),
            Change::Nodes(vec![("s1".into(), 7), ("s2".into(), 7)])
        );
    }

    #[test]
    fn document_change_dominates() {
        let log = ChangeLog::new(16, 8);
        let m = log.mark();
        log.record("s1", Hit::Nodes(vec![1]));
        log.record("s1", Hit::Document);
        log.record("s1", Hit::Nodes(vec![2]));
        assert_eq!(log.since(m), Change::Document);
    }

    #[test]
    fn too_many_nodes_falls_back_to_document() {
        let log = ChangeLog::new(64, 3);
        let m = log.mark();
        for n in 0..4 {
            log.record("s", Hit::Nodes(vec![n]));
        }
        assert_eq!(log.since(m), Change::Document);
    }

    #[test]
    fn evicted_baseline_is_conservative() {
        // 环形容量 2：基线早于被淘汰的条目 → 无法确认，按整页
        let log = ChangeLog::new(2, 8);
        let m = log.mark();
        for n in 0..5 {
            log.record("s", Hit::Nodes(vec![n]));
        }
        assert_eq!(log.since(m), Change::Document);
    }
}
