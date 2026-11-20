//! DOM 镜像通道（Phase 2 原型，设计见 docs/EDGE_BROWSER.md §9.2）：
//! 经 CDP 向当前观看会话注入页内序列化器，快照（JSON 节点树）与
//! 突变差分经 WS 转发给观看端镜像渲染。传输走现有 WS（TCP 可靠
//! 有序），seq 由本模块单调分配，缺口由观看端发 dom-resync 触发
//! 新快照——DOM 版的「关键帧请求」。

use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value};
use tokio::sync::{broadcast, Mutex};

use crate::browser::Browser;
use crate::human::HumanChannel;

/// 页内序列化器：节点 id 注册表 + 快照 + MutationObserver 差分 +
/// 表单/滚动旁路事件。经 __rigSend 绑定回传边缘。
const SCRIPT: &str = r##"(() => {
  if (window.__rigDom) { window.__rigDom.hello(); return; }
  const S = { next: 1, ids: new WeakMap(), byId: new Map(), obs: null, queue: [], timer: 0,
              cap: {}, dead: new Set(), capT: 0, blanks: new Map(),
              watched: new WeakSet(), fdepth: 0 };
  const send = (o) => { try { window.__rigSend(JSON.stringify(o)); } catch (e) {} };
  function rid(node) {
    let id = S.ids.get(node);
    if (!id) { id = S.next++; S.ids.set(node, id); S.byId.set(id, node); }
    return id;
  }
  // 资源 URL 一律在序列化时解析为绝对地址（以页面 URL 为基准）：
  // 镜像文档的源是观看页，相对/根相对路径若留到镜像端解析会落到
  // 观看端源上（实测 YouTube 的 /s/player/*.css 被请求到边缘地址
  // 吃 401）。data:/blob:/# 等非网络引用原样保留。
  function absUrl(v) {
    if (!v || /^(data:|blob:|javascript:|mailto:|tel:|#)/i.test(v)) return v;
    try { return new URL(v, location.href).href; } catch (e) { return v; }
  }
  function absSrcset(v) {
    if (!v) return v;
    return v.split(',').map((p) => {
      const t = p.trim().split(/\s+/);
      if (t.length) t[0] = absUrl(t[0]);
      return t.join(' ');
    }).join(', ');
  }
  // CSS 文本里的 url()/@import 以「样式表自身 URL」为基准改写——
  // 规则文本被搬进镜像的 <style> 后，基准会变成镜像文档地址，
  // 不改写同样落错源。
  function absCss(text, base) {
    if (!text) return text;
    return text
      .replace(/url\(\s*(['"]?)([^)'"]+)\1\s*\)/g, (m, q, u) => {
        const t = u.trim();
        if (!t || /^(data:|blob:|#)/i.test(t)) return m;
        try { return 'url("' + new URL(t, base).href + '")'; } catch (e) { return m; }
      })
      .replace(/@import\s+(['"])([^'"]+)\1/g, (m, q, u) => {
        try { return '@import "' + new URL(u, base).href + '"'; } catch (e) { return m; }
      });
  }
  function ser(node) {
    const id = rid(node);
    if (node.nodeType === 3) return { i: id, t: node.data };
    if (node.nodeType !== 1) return null;
    const tag = node.tagName;
    if (tag === 'SCRIPT' || tag === 'NOSCRIPT') return null;
    if (tag === 'CANVAS') {
      const r = node.getBoundingClientRect();
      return { i: id, g: 'CANVAS', a: {}, c: [], rect: [Math.round(r.width), Math.round(r.height)] };
    }
    const attrs = {};
    for (const at of node.attributes) { if (!/^on/i.test(at.name)) attrs[at.name] = at.value; }
    for (const k of ['href', 'src', 'poster', 'action', 'data', 'background']) {
      if (attrs[k]) attrs[k] = absUrl(attrs[k]);
    }
    if (attrs.srcset) attrs.srcset = absSrcset(attrs.srcset);
    const out = { i: id, g: tag, a: attrs, c: [] };
    // SVG 元素必须按命名空间重建（镜像端 createElementNS），
    // 否则 svg/path/use 落进 HTML 命名空间整体不渲染——现代
    // 站点的图标全是内联 SVG，实测 YouTube 镜像里所有图标
    // 按钮成空白圆圈即此故
    if (node.namespaceURI === 'http://www.w3.org/2000/svg') out.ns = 1;
    if (tag === 'INPUT' || tag === 'TEXTAREA' || tag === 'SELECT') { out.v = node.value; if (node.checked) out.k = 1; }
    if (tag === 'OPTION' && node.selected) out.k = 1;
    if (tag === 'VIDEO') {
      const r = node.getBoundingClientRect();
      out.rect = [Math.round(r.width), Math.round(r.height)];
      out.disp = getComputedStyle(node).display;
    }
    if (tag === 'IFRAME') {
      // 同源 iframe：内容文档整棵序列化进 f（镜像端注入其
      // contentDocument 渲染）；跨源 contentDocument 不可读、
      // 自然落空只有占位。内嵌文档的突变不走差分（观察器在
      // 主文档上），watchFrame 以重同步兜底；嵌套深度上限 4
      // 防自嵌页面无限递归。iframe 的 fallback 子节点不渲染、
      // 不序列化。
      let cross = false;
      if (S.fdepth < 4) {
        try {
          const d = node.contentDocument;
          if (d && d.documentElement) {
            S.fdepth++;
            try { const inner = ser(d.documentElement); if (inner) out.f = inner; }
            finally { S.fdepth--; }
            watchFrame(node, d);
          } else { cross = true; }
        } catch (e) { cross = true; }
      } else {
        try { cross = !node.contentDocument; } catch (e) { cross = true; }
      }
      if (cross) {
        // 跨源 iframe：内容受同源策略保护、序列化器读不到（非本
        // 系统缺陷）。标 x + rect，边缘走区域子流（按 rect 截图
        // 串流，见 __rigRegions），镜像端在占位块里贴图显示。
        // disp 必带：占位块必须保持原元素的行内/块级性质，否则
        // 后续行内流布局整体错位、镜像坐标与远端对不上（实测
        // iframe 被 div 占位后换行，点击全落空）。
        const r = node.getBoundingClientRect();
        out.x = 1;
        out.rect = [Math.round(r.width), Math.round(r.height)];
        out.disp = getComputedStyle(node).display;
      }
      return out;
    }
    for (const ch of node.childNodes) {
      const s = ser(ch);
      if (s) {
        if (tag === 'STYLE' && typeof s.t === 'string') s.t = absCss(s.t, location.href);
        out.c.push(s);
      }
    }
    return out;
  }
  // 同源 iframe 文档的监视：突变（节流 1.5s）与内部导航（load）
  // 都触发整页重同步——快照便宜且 iframe 内突变通常稀疏，比给
  // 每个内嵌文档单独维护差分流划算。表单值变化走与主文档相同
  // 的 val 旁路（id 注册表跨文档共享，镜像端按 id 定位即可）。
  function watchFrame(el, d) {
    if (S.watched.has(d)) return;
    S.watched.add(d);
    let last = 0;
    try {
      const mo = new MutationObserver(() => {
        const now = performance.now(); if (now - last < 1500) return; last = now;
        send({ kind: 'resync' });
      });
      mo.observe(d.documentElement, { subtree: true, childList: true, attributes: true, characterData: true });
    } catch (e) {}
    d.addEventListener('input', onInputVal, true);
    el.addEventListener('load', () => send({ kind: 'resync' }));
  }
  function onInputVal(e) {
    const t = e.target;
    if (t && (t.tagName === 'INPUT' || t.tagName === 'TEXTAREA' || t.tagName === 'SELECT')) {
      S.queue.push({ o: 'val', i: rid(t), v: t.value, k: t.checked ? 1 : 0 }); schedule();
    }
  }
  // 规则序列化（递归）：@import 同源可读则内联展开（跨源保留原行）；
  // @media (scripting:…) 必须改写——镜像 iframe 禁脚本，浏览器会把
  // scripting 报为 none，页面自带的无 JS 降级样式（如 OSM 在
  // @media (scripting: none) 里藏起 JS 驱动的侧栏）会误触发，
  // 与源页面（有 JS）的呈现分叉。none 分支整段丢弃、enabled
  // 分支去条件展开，镜像才与源页面的有-JS 呈现一致。
  function serRules(rules, depth, seen) {
    let out = '';
    for (const r of rules) {
      try {
        if (r.type === 3 && depth < 4) { // IMPORT_RULE
          const href = r.href || '';
          let done = false;
          if (r.styleSheet && !seen.has(href)) {
            try {
              const inner = absCss(serRules(r.styleSheet.cssRules, depth + 1, new Set(seen).add(href)), href);
              const media = r.media && r.media.mediaText;
              out += media ? '@media ' + media + ' {\n' + inner + '}\n' : inner;
              done = true;
            } catch (e) {}
          }
          if (!done) out += r.cssText + '\n';
        } else if (r.type === 4) { // MEDIA_RULE
          const cond = r.conditionText || (r.media && r.media.mediaText) || '';
          // 条件若仅由媒体类型与 scripting 特征以 and 组成，则它是纯
          // scripting 门控：none 分支丢弃、enabled 分支去条件展开；
          // 混有其他特征的条件原样保留（镜像里按真实环境求值）。
          const parts = cond.split(/\s+and\s+/i).map((s) => s.trim()).filter(Boolean);
          const rest = parts.filter((p) => !/^(only\s+)?(screen|all)$/i.test(p) && !/scripting/i.test(p));
          const pureScripting = parts.length > 0 && rest.length === 0 && /scripting/i.test(cond);
          if (pureScripting && /scripting\s*:\s*none/.test(cond)) {
            // 丢弃无-JS 降级分支
          } else if (pureScripting && /scripting\s*:\s*enabled/.test(cond)) {
            out += serRules(r.cssRules, depth, seen);
          } else {
            out += '@media ' + cond + ' {\n' + serRules(r.cssRules, depth, seen) + '}\n';
          }
        } else {
          out += r.cssText + '\n';
        }
      } catch (e) {}
    }
    return out;
  }
  function collectCss() {
    let css = ''; const links = [];
    for (const sh of document.styleSheets) {
      try {
        const inner = absCss(serRules(sh.cssRules, 0, new Set()), sh.href || location.href);
        // 样式表自身的 media（如 <link media="print">）必须保留：
        // 丢了它，打印样式表的规则会在镜像里无条件生效
        // （实测 OSM 的 print 表把侧栏/页眉全藏了）。
        const media = sh.media && sh.media.mediaText;
        css += (media && media !== 'all') ? '@media ' + media + ' {\n' + inner + '}\n' : inner;
      }
      catch (e) { if (sh.href) links.push(sh.href); }
    }
    return { css, links };
  }
  function snapshot() {
    S.cap = {}; // 镜像重建后占位块是新的：强制下一拍重发 canvas 帧
    const cc = collectCss();
    return { url: location.href, title: document.title, w: window.innerWidth, h: window.innerHeight,
             sx: Math.round(window.scrollX), sy: Math.round(window.scrollY),
             mode: document.compatMode,
             css: cc.css, links: cc.links, tree: ser(document.documentElement) };
  }
  // canvas 子流（快照式）：1s 一拍，只发变化的帧。空白帧（与同尺寸
  // 新建 canvas 的数据逐字相同）不发——WebGL 无 preserveDrawingBuffer
  // 时 toDataURL 恒为空白，正好落入此路，占位块保持。跨源污染的
  // canvas toDataURL 抛错，标记 dead 不再重试。尺寸上限 1024，
  // 且单帧 data URL 超过约 1.5MB（照片级内容）不发——子流是
  // 语义补充不是视频通道，大帧会挤占差分通道。
  function blankRef(w, h) {
    const key = w + 'x' + h;
    if (!S.blanks.has(key)) {
      const c = document.createElement('canvas'); c.width = w; c.height = h;
      let d = null;
      try { d = c.toDataURL('image/png'); } catch (e) {}
      S.blanks.set(key, d);
    }
    return S.blanks.get(key);
  }
  function capCanvases() {
    for (const [id, node] of S.byId) {
      if (S.dead.has(id) || !node || node.tagName !== 'CANVAS' || !node.isConnected) continue;
      const w = node.width, h = node.height;
      if (!w || !h || w > 1024 || h > 1024) continue;
      let d;
      try { d = node.toDataURL('image/png'); } catch (e) { S.dead.add(id); continue; }
      if (d.length > 1500000) continue;
      if (S.cap[id] === undefined && d === blankRef(w, h)) continue;
      if (S.cap[id] !== d) { S.cap[id] = d; send({ kind: 'canvas', i: id, d }); }
    }
  }
  function flush() {
    if (!S.queue.length) return;
    // 增量上限（R-08/R-20）：突变风暴下宁可整帧重同步，
    // 不让无界队列拖垮页面与通道
    if (S.queue.length > 3000) { S.queue = []; send({ kind: 'resync' }); return; }
    const ops = S.queue; S.queue = []; send({ kind: 'delta', ops });
  }
  // 删除一棵已移除子树的 id 映射（R-08：只发 rm 不清映射，
  // 被删节点会被 byId/ids 强引用到页面关闭，SPA 长开必漏）
  function dropSubtree(root) {
    const walk = [root];
    while (walk.length) {
      const n = walk.pop();
      const id = S.ids.get(n);
      if (id !== undefined) { S.ids.delete(n); S.byId.delete(id); delete S.cap[id]; }
      if (n.childNodes) for (const c of n.childNodes) walk.push(c);
    }
  }
  function schedule() { if (!S.timer) S.timer = setTimeout(() => { S.timer = 0; flush(); }, 30); }
  function onMutations(recs) {
    for (const r of recs) {
      if (document.head && (r.target === document.head || (r.target.nodeType === 1 && document.head.contains(r.target)))) {
        send({ kind: 'resync' }); return;
      }
      if (r.type === 'childList') {
        for (const n of r.removedNodes) { const id = S.ids.get(n); if (id) S.queue.push({ o: 'rm', i: id }); dropSubtree(n); }
        for (const n of r.addedNodes) {
          const s = ser(n);
          if (s) {
            const nb = n.nextSibling ? S.ids.get(n.nextSibling) : undefined;
            S.queue.push({ o: 'add', p: rid(r.target), b: nb || 0, n: s });
          }
        }
      } else if (r.type === 'attributes') {
        let v = r.target.getAttribute(r.attributeName);
        // 差分里的资源属性同样要绝对化（与快照同一口径）
        if (v && ['href', 'src', 'poster', 'action', 'data', 'background'].includes(r.attributeName)) v = absUrl(v);
        if (v && r.attributeName === 'srcset') v = absSrcset(v);
        S.queue.push({ o: 'at', i: rid(r.target), n: r.attributeName, v });
      } else if (r.type === 'characterData') {
        S.queue.push({ o: 'tx', i: rid(r.target), v: r.target.data });
      }
    }
    schedule();
  }
  document.addEventListener('input', onInputVal, true);
  let rzT = 0;
  window.addEventListener('resize', () => {
    // 视口变化会整体重排版：防抖后请边缘重发快照（镜像几何以快照为准）
    const now = performance.now(); if (now - rzT < 300) return; rzT = now;
    setTimeout(() => send({ kind: 'resync' }), 350);
  });
  let scrT = 0, scrTrail = 0;
  const pushScr = (t) => {
    if (t === document || t === document.documentElement || t === document.body) {
      S.queue.push({ o: 'scr', i: 0, x: Math.round(window.scrollX), y: Math.round(window.scrollY) });
    } else if (t && t.nodeType === 1) {
      S.queue.push({ o: 'scr', i: rid(t), x: Math.round(t.scrollLeft), y: Math.round(t.scrollTop) });
    }
    schedule();
  };
  document.addEventListener('scroll', (e) => {
    const now = performance.now();
    const t = e.target;
    if (now - scrT < 50) {
      // 节流窗口内的事件不能直接丢：滚动停稳的最终位置往往就在
      // 最后一发里，丢了镜像会停在中间值（实测滚到底镜像停半途）。
      // 补一发尾随推送，读的是触发时的当前滚动位置
      if (!scrTrail) scrTrail = setTimeout(() => { scrTrail = 0; scrT = performance.now(); pushScr(t); }, 60);
      return;
    }
    scrT = now;
    pushScr(t);
  }, true);
  // 区域子流目标清单（边缘轮询调用）：跨源 iframe（序列化器读
  // 不到内容）与 video 元素——这两类镜像里只有占位，边缘按
  // rect 对页面表面截图串流补齐。rect 为视口相对 CSS px；
  // video 带 p（播放中）与 ct（当前时刻，停稳签名用）。
  function regions() {
    const out = [];
    let frames, videos;
    try { frames = document.querySelectorAll('iframe'); videos = document.querySelectorAll('video'); }
    catch (e) { return out; }
    for (const f of frames) {
      let cross = false;
      try { cross = !f.contentDocument; } catch (e) { cross = true; }
      if (!cross) continue;
      const r = f.getBoundingClientRect();
      if (r.width < 8 || r.height < 8) continue;
      out.push({ i: rid(f), k: 'x', x: Math.round(r.left), y: Math.round(r.top), w: Math.round(r.width), h: Math.round(r.height) });
    }
    for (const v of videos) {
      const r = v.getBoundingClientRect();
      if (r.width < 8 || r.height < 8) continue;
      out.push({ i: rid(v), k: 'v', x: Math.round(r.left), y: Math.round(r.top), w: Math.round(r.width), h: Math.round(r.height), p: (!v.paused && !v.ended) ? 1 : 0, ct: Math.round((v.currentTime || 0) * 10) });
    }
    return out;
  }
  // 按节点 id 取图片像素（观看端右键「复制图片」）：镜像坐标与
  // 远端布局可能漂移（行内流换行点不同），按坐标命中不可靠；
  // 按 id 在注册表里定位元素本体则永远准确。非 IMG 或像素被
  // 跨源污染时回空串。
  function imgData(i) {
    const el = S.byId.get(i);
    if (!el || el.tagName !== 'IMG' || !el.naturalWidth) return '';
    try {
      const c = document.createElement('canvas');
      c.width = el.naturalWidth; c.height = el.naturalHeight;
      c.getContext('2d').drawImage(el, 0, 0);
      return c.toDataURL('image/png').split(',')[1];
    } catch (e) { return ''; }
  }
  window.__rigDom = {
    snapshot,
    regions,
    imgData,
    hello() { send({ kind: 'installed' }); },
    start() {
      if (!S.obs) {
        S.obs = new MutationObserver(onMutations);
        S.obs.observe(document.documentElement, { subtree: true, childList: true, attributes: true, characterData: true });
      }
      if (!S.capT) { S.capT = setInterval(capCanvases, 1000); setTimeout(capCanvases, 300); }
    },
    stop() {
      if (S.obs) { S.obs.disconnect(); S.obs = null; }
      if (S.capT) { clearInterval(S.capT); S.capT = 0; }
    }
  };
  send({ kind: 'installed' });
})()"##;

/// 浏览器内部页 scheme：WebUI 界面在 shadow DOM 内、DOM 镜像
/// 拿不到实内容，一律走视频模式（见 snapshot 的把关）
fn is_internal_url(u: &str) -> bool {
    const SCHEMES: [&str; 9] = [
        "chrome://",
        "chrome-extension://",
        "chrome-untrusted://",
        "chrome-search://",
        "about:",
        "devtools://",
        "view-source:",
        "edge://",
        "brave://",
    ];
    SCHEMES.iter().any(|s| u.starts_with(s))
}

pub struct DomChannel {
    browser: Arc<Browser>,
    human: Arc<HumanChannel>,
    msgs: broadcast::Sender<String>,
    seq: AtomicU64,
    viewers: AtomicUsize,
    active: AtomicBool,
    /// 已完成绑定+脚本注册的会话（避免重复 addScript）
    setup_for: Mutex<Option<String>>,
    pump: Mutex<Option<tokio::task::JoinHandle<()>>>,
    /// 区域子流的当前采集目标会话（泵按视图变化维护，区域任务
    /// 只读）——F-01：区域采集独立任务，与差分泵解耦
    region_session: Mutex<Option<String>>,
    region_pump: Mutex<Option<tokio::task::JoinHandle<()>>>,
}

impl DomChannel {
    pub fn new(browser: Arc<Browser>, human: Arc<HumanChannel>) -> Arc<DomChannel> {
        let (msgs, _) = broadcast::channel(256);
        Arc::new(DomChannel {
            browser,
            human,
            msgs,
            seq: AtomicU64::new(0),
            viewers: AtomicUsize::new(0),
            active: AtomicBool::new(false),
            setup_for: Mutex::new(None),
            pump: Mutex::new(None),
            region_session: Mutex::new(None),
            region_pump: Mutex::new(None),
        })
    }

    pub fn subscribe(&self) -> broadcast::Receiver<String> {
        self.msgs.subscribe()
    }

    /// O01：当前 DOM 订阅计数（/health dom_viewers）
    pub fn viewer_count(&self) -> usize {
        self.viewers.load(Ordering::SeqCst)
    }

    fn next_seq(&self) -> u64 {
        self.seq.fetch_add(1, Ordering::SeqCst) + 1
    }

    fn broadcast(&self, v: Value) {
        let _ = self.msgs.send(v.to_string());
    }

    /// 一个观看连接进入 DOM 模式
    pub async fn viewer_start(self: &Arc<Self>) {
        let first = self.viewers.fetch_add(1, Ordering::SeqCst) == 0;
        if first {
            self.active.store(true, Ordering::SeqCst);
            self.ensure_pump().await;
        }
        if let Some(s) = self.human.current_view().await {
            // 每次进入都确保页内观察者在跑：页面导航/刷新后序列化
            // 器是新实例（观察者未启动），只靠首个观看者那次 start
            // 不够——计数一旦因断线泄漏，后来者永远走补快照分支，
            // 增量会静默死亡（实测）。setup_session 对已装会话只
            // 补一发 start() eval，开销可忽略
            self.setup_session(&s).await;
            self.snapshot(&s).await;
        } else if first {
            self.broadcast(
                json!({"type": "dom-state", "state": "error", "reason": "人通道未启动"}),
            );
        }
    }

    /// 一个观看连接退出 DOM 模式
    pub async fn viewer_stop(&self) {
        // 计数下限为 0：未配对的 stop（重连后的旧页面、重复消息）
        // 不许把计数减成负数，否则后续 start 的首入判定永久错位。
        // CAS 循环手写（fetch_update 的继任 API 超出仓库 MSRV）
        let mut took_last = false;
        let mut cur = self.viewers.load(Ordering::SeqCst);
        while cur > 0 {
            match self
                .viewers
                .compare_exchange(cur, cur - 1, Ordering::SeqCst, Ordering::SeqCst)
            {
                Ok(_) => {
                    took_last = cur == 1;
                    break;
                }
                Err(actual) => cur = actual,
            }
        }
        if took_last {
            self.active.store(false, Ordering::SeqCst);
            if let Some(s) = self.human.current_view().await {
                let _ = self
                    .browser
                    .eval_json(&s, "window.__rigDom && window.__rigDom.stop()")
                    .await;
            }
        }
    }

    /// 观看端请求重同步（seq 缺口/手动）
    pub async fn resync(&self) {
        if !self.active.load(Ordering::SeqCst) {
            return;
        }
        if let Some(s) = self.human.current_view().await {
            self.setup_session(&s).await;
            self.snapshot(&s).await;
        }
    }

    async fn ensure_pump(self: &Arc<Self>) {
        let mut slot = self.pump.lock().await;
        if slot.is_some() {
            return;
        }
        // F-01：区域采集独立任务（100ms 节拍）——截图慢只拖慢
        // 区域自身帧率，不再堵差分广播（此前 sweep 内联于泵的
        // select 循环，一次慢截图冻结全部增量投递）
        {
            let me_r = self.clone();
            let mut rslot = self.region_pump.lock().await;
            if rslot.is_none() {
                *rslot = Some(tokio::spawn(async move {
                    let mut tick = tokio::time::interval(Duration::from_millis(100));
                    // D04：region 缓存按（会话, 区域 id）隔离——导航/换文档后
                    // 页内序列化器 id 从 1 重排，旧会话条目必须淘汰，不把
                    // 旧页截图贴给新文档；总量设上限防堆积
                    let mut state: std::collections::HashMap<
                        (String, i64),
                        (String, std::time::Instant),
                    > = std::collections::HashMap::new();
                    loop {
                        tick.tick().await;
                        if !me_r.active.load(Ordering::SeqCst) {
                            continue;
                        }
                        let target = me_r.region_session.lock().await.clone();
                        if let Some(s) = target {
                            me_r.region_sweep(&s, &mut state).await;
                        }
                    }
                }));
            }
        }
        let me = self.clone();
        *slot = Some(tokio::spawn(async move {
            let mut rx = me.browser.cdp.subscribe();
            let mut tick = tokio::time::interval(Duration::from_millis(300));
            let mut last_view: Option<String> = None;
            loop {
                tokio::select! {
                    ev = rx.recv() => {
                        let Ok(ev) = ev else { continue };
                        if ev.method != "Runtime.bindingCalled" { continue; }
                        if ev.params.get("name").and_then(Value::as_str) != Some("__rigSend") { continue; }
                        if !me.active.load(Ordering::SeqCst) { continue; }
                        let cur = me.human.current_view().await;
                        if ev.session.is_some() && cur.is_some() && ev.session != cur { continue; }
                        let Some(payload) = ev.params.get("payload").and_then(Value::as_str) else { continue };
                        let Ok(p) = serde_json::from_str::<Value>(payload) else { continue };
                        match p.get("kind").and_then(Value::as_str) {
                            Some("delta") => {
                                if let Some(ops) = p.get("ops") {
                                    me.broadcast(json!({"type": "dom-delta", "seq": me.next_seq(), "ops": ops}));
                                }
                            }
                            Some("canvas") => {
                                // canvas 子流帧：无 seq（带外子流，按节点
                                // id 投递，镜像未含该节点时观看端忽略）
                                if let (Some(i), Some(d)) = (p.get("i"), p.get("d")) {
                                    me.broadcast(json!({"type": "dom-canvas", "i": i, "d": d}));
                                }
                            }
                            Some("installed") | Some("resync") => {
                                if let Some(s) = cur {
                                    // installed = 页面导航/刷新后序列化器
                                    // 重新注入：新实例的观察者尚未启动，
                                    // 只补快照会让增量静默死亡，必须先
                                    // setup（内含 start() 补发）
                                    me.setup_session(&s).await;
                                    me.snapshot(&s).await;
                                }
                            }
                            _ => {}
                        }
                    }
                    _ = tick.tick() => {
                        if !me.active.load(Ordering::SeqCst) { continue; }
                        let cur = me.human.current_view().await;
                        if cur != last_view {
                            last_view = cur.clone();
                            if let Some(s) = cur {
                                me.setup_session(&s).await;
                                me.snapshot(&s).await;
                            }
                        }
                        // 区域采集目标交给独立任务（F-01）
                        *me.region_session.lock().await = last_view.clone();
                    }
                }
            }
        }));
    }

    /// 区域子流一轮：问页内 __rigRegions 要目标清单，按各自节奏
    /// 截图广播。跨源 iframe 每 ~1.2s 或 rect 变化时刷一帧（验证
    /// 码等内容足够）；video 播放中每 ~150ms 一帧（F-02，原
    /// 280ms 只有 ~3.6fps，镜像里视频明显跳帧）、暂停时仅在
    /// 位置/时刻签名变化时刷（seek/换源）。本方法由独立区域
    /// 任务调用（F-01），不在差分泵的循环里。
    async fn region_sweep(
        &self,
        session: &str,
        state: &mut std::collections::HashMap<(String, i64), (String, std::time::Instant)>,
    ) {
        let Ok(v) = self
            .browser
            .eval_json(session, "window.__rigDom ? window.__rigDom.regions() : []")
            .await
        else {
            return;
        };
        let Some(list) = v.as_array() else { return };
        for t in list {
            let (Some(i), Some(k)) = (
                t.get("i").and_then(Value::as_i64),
                t.get("k").and_then(Value::as_str),
            ) else {
                continue;
            };
            let g = |key: &str| t.get(key).and_then(Value::as_i64).unwrap_or(0);
            let (x, y, w, h) = (g("x"), g("y"), g("w"), g("h"));
            if w < 8 || h < 8 {
                continue;
            }
            let sig = format!("{x},{y},{w},{h},{}", g("ct"));
            let playing = g("p") == 1;
            let key = (session.to_string(), i);
            let due = match state.get(&key) {
                None => true,
                Some((ps, at)) => {
                    if k == "x" {
                        ps != &sig || at.elapsed() > Duration::from_millis(1200)
                    } else if playing {
                        at.elapsed() > Duration::from_millis(150)
                    } else {
                        ps != &sig
                    }
                }
            };
            if !due {
                continue;
            }
            if let Some(data) = self.browser.capture_region(session, x, y, w, h).await {
                state.insert(key, (sig, std::time::Instant::now()));
                // D04：只保留当前会话条目（旧会话的 id 会重排复用），
                // 总量过大时整清（下轮重建，短暂无图但不錯贴）
                state.retain(|(s, _), _| s == session);
                if state.len() > 2048 {
                    state.clear();
                }
                self.broadcast(json!({"type": "dom-region", "i": i, "d": data}));
            }
        }
    }

    /// 给会话装绑定与脚本（同一会话只装一次），并启动页内观察
    async fn setup_session(&self, session: &str) {
        {
            let done = self.setup_for.lock().await;
            if done.as_deref() == Some(session) {
                let _ = self
                    .browser
                    .eval_json(session, "window.__rigDom && window.__rigDom.start()")
                    .await;
                return;
            }
        }
        let cdp = &self.browser.cdp;
        let _ = cdp.call(Some(session), "Runtime.enable", json!({})).await;
        let _ = cdp
            .call(
                Some(session),
                "Runtime.addBinding",
                json!({"name": "__rigSend"}),
            )
            .await;
        let _ = cdp.call(Some(session), "Page.enable", json!({})).await;
        let _ = cdp
            .call(
                Some(session),
                "Page.addScriptToEvaluateOnNewDocument",
                json!({"source": SCRIPT}),
            )
            .await;
        let _ = self.browser.eval_json(session, SCRIPT).await;
        let _ = self
            .browser
            .eval_json(session, "window.__rigDom && window.__rigDom.start()")
            .await;
        *self.setup_for.lock().await = Some(session.to_string());
    }

    /// 取一份全量快照并广播（DOM 版关键帧）
    async fn snapshot(&self, session: &str) {
        // 浏览器内部页（设置/下载/扩展等 WebUI）：界面全在
        // shadow DOM 里，序列化器只能拿到空壳树——镜像出来是
        // 一片残页（2026-11-20 LIU 实测「无法显示」）。这类页
        // 面一律不产快照，直接报状态让观看端回落视频模式
        // （screencast 对 WebUI 正常出帧）。在此一处把关，
        // viewer_start/resync/感知泵三条来路全覆盖。
        if let Ok(u) = self.browser.eval_json(session, "location.href").await {
            if let Some(u) = u.as_str() {
                if is_internal_url(u) {
                    self.broadcast(json!({"type": "dom-state", "state": "error", "reason": "浏览器内部页面不支持 DOM 镜像，已切视频模式"}));
                    return;
                }
            }
        }
        let t0 = std::time::Instant::now();
        let r = self
            .browser
            .eval_json(
                session,
                "window.__rigDom ? window.__rigDom.snapshot() : null",
            )
            .await;
        crate::metrics::global().dom_flush.record(t0.elapsed());
        match r {
            Ok(Value::Object(mut snap)) => {
                snap.insert("type".into(), json!("dom-snapshot"));
                snap.insert("seq".into(), json!(self.next_seq()));
                self.broadcast(Value::Object(snap));
            }
            Ok(_) => {
                self.broadcast(json!({"type": "dom-state", "state": "error", "reason": "页面无序列化器（特殊页面不支持 DOM 镜像）"}));
            }
            Err(e) => {
                self.broadcast(json!({"type": "dom-state", "state": "error", "reason": format!("快照失败: {e}")}));
            }
        }
    }
}
