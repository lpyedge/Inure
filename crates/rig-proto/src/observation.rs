//! Observation / Action 契约：感知层与 AI、Viewer 之间的统一语言。
//!
//! 设计见 `docs/PERCEPTION.md`。这里只放**纯数据类型和纯函数**，不含任何 IO，
//! 以便 Observer（Connector 内或 Sidecar）、Gateway、MCP 层、评估框架共用同一份定义。
//!
//! 几条不能破坏的约定：
//! 1. **坐标统一为"逻辑像素"**（见 [`Space`]）。设备像素、DPI 缩放、画面缩放都在边缘转换，
//!    AI 和 Viewer 永远只见逻辑坐标。
//! 2. **动作引用元素和帧**，不是裸坐标。目标元素自该帧以来发生变化 → 拒绝（见 [`check_element_target`]）。
//! 3. **所有来自被控端的文本都是不可信数据**，类型层面固定为 [`ContentTrust::Untrusted`]。
//! 4. **密码等敏感值永不出现在 Observation 中**（见 [`Observation::redact_sensitive`]）。

use serde::{Deserialize, Serialize};

/// 帧版本号。由 Observer 单调递增分配。语义快照、视频帧、人的标注都用它对齐。
pub type FrameId = u64;

/// 判断"元素是否移动/变化"时默认的像素容差（逻辑像素）。
pub const DEFAULT_TOLERANCE_PX: f32 = 2.0;

// ───────────────────────────── 坐标 ─────────────────────────────

/// 参考坐标空间。原点在被观察表面的左上角，单位是**逻辑像素**（Web 的 CSS 像素、Android 的 dp 换算后的值）。
///
/// `scale` 是"物理像素 / 逻辑像素"（Web 的 devicePixelRatio）。只有边缘在向设备注入输入、
/// 或把图像像素换算成逻辑坐标时才使用它。
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct Space {
    pub width: f32,
    pub height: f32,
    pub scale: f32,
}

impl Space {
    pub fn is_valid(&self) -> bool {
        [self.width, self.height, self.scale]
            .iter()
            .all(|v| v.is_finite() && *v > 0.0)
    }

    pub fn to_physical(&self, x: f32, y: f32) -> (f32, f32) {
        (x * self.scale, y * self.scale)
    }

    pub fn to_logical(&self, x: f32, y: f32) -> (f32, f32) {
        (x / self.scale, y / self.scale)
    }

    pub fn contains(&self, x: f32, y: f32) -> bool {
        x >= 0.0 && y >= 0.0 && x < self.width && y < self.height
    }
}

/// 包围盒 `[x1, y1, x2, y2]`，逻辑像素，左闭右开。
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct BBox(pub [f32; 4]);

impl BBox {
    pub fn width(&self) -> f32 {
        self.0[2] - self.0[0]
    }

    pub fn height(&self) -> f32 {
        self.0[3] - self.0[1]
    }

    pub fn area(&self) -> f32 {
        self.width().max(0.0) * self.height().max(0.0)
    }

    pub fn contains(&self, x: f32, y: f32) -> bool {
        x >= self.0[0] && x < self.0[2] && y >= self.0[1] && y < self.0[3]
    }

    pub fn center(&self) -> (f32, f32) {
        ((self.0[0] + self.0[2]) / 2.0, (self.0[1] + self.0[3]) / 2.0)
    }

    pub fn is_valid(&self) -> bool {
        self.0.iter().all(|v| v.is_finite()) && self.0[2] >= self.0[0] && self.0[3] >= self.0[1]
    }

    pub fn approx_eq(&self, other: &BBox, tol: f32) -> bool {
        self.0
            .iter()
            .zip(other.0.iter())
            .all(|(a, b)| (a - b).abs() <= tol)
    }
}

// ───────────────────────────── 元素 ─────────────────────────────

/// 元素状态。封闭枚举，只增不改。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ElementState {
    Disabled,
    Focused,
    Checked,
    Selected,
    Expanded,
    Collapsed,
    Readonly,
    Required,
    Busy,
    /// 在视口之外（需要滚动才可见）。默认不出现在 Observation 中，见 `ObserveRequest::include_offscreen`。
    Offscreen,
    /// 密码输入框。其 `value` 必须被脱敏。
    Password,
    /// 其他敏感字段（一次性验证码、银行卡号等）。其 `value` 必须被脱敏。
    Sensitive,
}

/// 这个元素的信息从哪里来。AI 据此判断可信程度。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Origin {
    /// 系统无障碍接口（浏览器 AX 树、Android AccessibilityService、UIA、AT-SPI）。
    A11y,
    /// DOM 或等价的结构化文档。
    Dom,
    /// 文字识别。
    Ocr,
    /// 视觉模型或检测器。
    Vision,
}

/// 视为"可交互"的规范化角色。Observer 必须把各平台的角色名映射到这套词表（可以新增，不能改义）。
pub const INTERACTIVE_ROLES: &[&str] = &[
    "button",
    "link",
    "textbox",
    "searchbox",
    "checkbox",
    "radio",
    "switch",
    "combobox",
    "listbox",
    "option",
    "menuitem",
    "tab",
    "slider",
    "spinbutton",
];

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Element {
    /// 形如 "e12"。同一元素在连续的帧之间应保持同一个 id（由 Observer 跟踪保证）。
    pub id: String,
    /// 规范化角色，例如 button / link / textbox / heading / text。
    pub role: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// 当前值。密码/敏感元素必须为 None。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bbox: Option<BBox>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub states: Vec<ElementState>,
    pub origin: Origin,
    /// 0..=1。无障碍/DOM 来源为 1.0；OCR/视觉来源按实际置信度。
    pub confidence: f32,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub children: Vec<Element>,
}

impl Element {
    pub fn new(id: impl Into<String>, role: impl Into<String>, origin: Origin) -> Self {
        Self {
            id: id.into(),
            role: role.into(),
            name: None,
            value: None,
            bbox: None,
            states: Vec::new(),
            origin,
            confidence: 1.0,
            children: Vec::new(),
        }
    }

    pub fn with_name(mut self, name: impl Into<String>) -> Self {
        self.name = Some(name.into());
        self
    }

    pub fn with_value(mut self, value: impl Into<String>) -> Self {
        self.value = Some(value.into());
        self
    }

    pub fn with_bbox(mut self, b: [f32; 4]) -> Self {
        self.bbox = Some(BBox(b));
        self
    }

    pub fn with_states(mut self, s: &[ElementState]) -> Self {
        self.states = s.to_vec();
        self
    }

    pub fn with_child(mut self, c: Element) -> Self {
        self.children.push(c);
        self
    }

    pub fn has(&self, s: ElementState) -> bool {
        self.states.contains(&s)
    }

    pub fn is_interactive_role(&self) -> bool {
        INTERACTIVE_ROLES.contains(&self.role.as_str())
    }

    /// 有位置、未禁用、在视口内。
    pub fn is_interactable(&self) -> bool {
        self.bbox.is_some()
            && !self.has(ElementState::Disabled)
            && !self.has(ElementState::Offscreen)
    }

    pub fn walk<'a>(&'a self, f: &mut dyn FnMut(&'a Element)) {
        f(self);
        for c in &self.children {
            c.walk(f);
        }
    }

    pub fn walk_mut(&mut self, f: &mut dyn FnMut(&mut Element)) {
        f(self);
        for c in &mut self.children {
            c.walk_mut(f);
        }
    }
}

/// 判断同一个元素在两帧之间是否"没变"：角色、名称、状态集合、位置（在容差内）都一致。
///
/// 刻意忽略：`value`（输入内容会变，元素仍是同一个）、`Focused` 与 `Busy` 状态（易变，不影响点击目标）。
pub fn element_unchanged(a: &Element, b: &Element, tol_px: f32) -> bool {
    fn stable(s: &[ElementState]) -> Vec<ElementState> {
        s.iter()
            .copied()
            .filter(|x| !matches!(x, ElementState::Focused | ElementState::Busy))
            .collect()
    }
    let (sa, sb) = (stable(&a.states), stable(&b.states));
    let same_states = sa.iter().all(|s| sb.contains(s)) && sb.iter().all(|s| sa.contains(s));
    let same_box = match (a.bbox, b.bbox) {
        (Some(x), Some(y)) => x.approx_eq(&y, tol_px),
        (None, None) => true,
        _ => false,
    };
    a.role == b.role && a.name == b.name && same_states && same_box
}

// ───────────────────────────── Observation ─────────────────────────────

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ObservationSource {
    Cdp,
    Android,
    Terminal,
    /// 纯像素兜底流水线（OCR / 检测 / 视觉）。
    Pixels,
}

/// 内容可信度。**只有 `Untrusted` 一个取值，这是刻意的**：
/// 被控端的任何文本（网页、终端输出、应用界面）都可能包含针对 AI 的指令，
/// 类型层面不提供"可信"选项，防止有人在某条路径上把它当指令使用。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContentTrust {
    #[default]
    Untrusted,
}

/// 与某一帧对应的图像（或视频帧）的引用。`width`/`height` 是图像的**物理像素**尺寸。
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ImageRef {
    pub uri: String,
    pub width: u32,
    pub height: u32,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Observation {
    pub frame_id: FrameId,
    pub source: ObservationSource,
    pub space: Space,
    /// 根元素列表（树）。
    pub elements: Vec<Element>,
    /// 纯文本视图（终端屏幕、页面可见文字）。`ObserveMode::Text` 下主要使用它。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image: Option<ImageRef>,
    #[serde(default)]
    pub content_trust: ContentTrust,
}

impl Observation {
    pub fn new(frame_id: FrameId, source: ObservationSource, space: Space) -> Self {
        Self {
            frame_id,
            source,
            space,
            elements: Vec::new(),
            text: None,
            image: None,
            content_trust: ContentTrust::Untrusted,
        }
    }

    pub fn each_element<'a>(&'a self, f: &mut dyn FnMut(&'a Element)) {
        for e in &self.elements {
            e.walk(f);
        }
    }

    pub fn find(&self, id: &str) -> Option<&Element> {
        let mut found: Option<&Element> = None;
        self.each_element(&mut |e| {
            if found.is_none() && e.id == id {
                found = Some(e);
            }
        });
        found
    }

    fn best_at(&self, x: f32, y: f32, pred: impl Fn(&Element) -> bool) -> Option<&Element> {
        let mut best: Option<(f32, &Element)> = None;
        self.each_element(&mut |e| {
            if !pred(e) {
                return;
            }
            let Some(b) = e.bbox else { return };
            if !b.contains(x, y) {
                return;
            }
            let area = b.area();
            // 面积最小者胜；面积相同取文档顺序靠后者（通常在更上层）。
            match best {
                Some((best_area, _)) if area > best_area => {}
                _ => best = Some((area, e)),
            }
        });
        best.map(|(_, e)| e)
    }

    /// 人在 Viewer 上点击 (x, y)（逻辑坐标）→ 命中的最小元素。跳过视口外元素。
    pub fn hit_test(&self, x: f32, y: f32) -> Option<&Element> {
        self.best_at(x, y, |e| !e.has(ElementState::Offscreen))
    }

    /// 同上，但只考虑可交互元素（点在按钮里的文字上，返回按钮而不是文字）。
    pub fn hit_test_interactive(&self, x: f32, y: f32) -> Option<&Element> {
        self.best_at(x, y, |e| e.is_interactive_role() && e.is_interactable())
    }

    /// 清除密码/敏感元素的 `value`，返回清除的个数。**每个 Observer 在输出前必须调用。**
    pub fn redact_sensitive(&mut self) -> usize {
        let mut n = 0;
        for e in &mut self.elements {
            e.walk_mut(&mut |el| {
                if (el.has(ElementState::Password) || el.has(ElementState::Sensitive))
                    && el.value.take().is_some()
                {
                    n += 1;
                }
            });
        }
        n
    }

    /// 需要在图像/视频上打码的区域（密码与敏感元素的包围盒）。
    pub fn sensitive_regions(&self) -> Vec<BBox> {
        let mut v = Vec::new();
        self.each_element(&mut |e| {
            if e.has(ElementState::Password) || e.has(ElementState::Sensitive) {
                if let Some(b) = e.bbox {
                    v.push(b);
                }
            }
        });
        v
    }

    /// 图像上的像素点 → 逻辑坐标。没有图像或尺寸为 0 时返回 None。
    pub fn image_to_logical(&self, px: f32, py: f32) -> Option<(f32, f32)> {
        let img = self.image.as_ref()?;
        if img.width == 0 || img.height == 0 || !self.space.is_valid() {
            return None;
        }
        Some((
            px * self.space.width / img.width as f32,
            py * self.space.height / img.height as f32,
        ))
    }
}

/// 增量更新。以元素 id 为键：id 已存在则**整棵子树替换**；不存在则追加到 `parent` 之下
/// （`parent` 为 None 表示根层）。新增元素的相对顺序不保证，需要顺序时请发完整快照。
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ObservationDelta {
    /// 增量所基于的帧。接收方当前帧不是它时，必须丢弃增量并请求完整快照。
    pub base_frame: FrameId,
    pub frame_id: FrameId,
    pub upserts: Vec<ElementUpsert>,
    pub removed: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ElementUpsert {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<String>,
    pub element: Element,
}

/// 元素自身字段是否相同（不含 children）。diff 的下钻判据：
/// 自身没变就只递归比较子节点，避免一处叶子变化把整棵树打包成 upsert。
fn element_self_eq(a: &Element, b: &Element) -> bool {
    a.role == b.role
        && a.name == b.name
        && a.value == b.value
        && a.bbox == b.bbox
        && a.states == b.states
        && a.origin == b.origin
        && a.confidence == b.confidence
}

/// 计算两帧之间的增量。语义与 `ObservationDelta` 的定义一致：
/// 自身字段变了、父节点变了、或新增的元素 → 整棵子树 upsert；
/// 自身没变的元素只递归下钻到变化点。消失的元素只报最顶层
/// （其后代随子树一并消失，不逐个列）。
pub fn diff_observations(old: &Observation, new: &Observation) -> ObservationDelta {
    use std::collections::{HashMap, HashSet};
    fn index<'a>(
        e: &'a Element,
        parent: Option<&'a str>,
        by_id: &mut HashMap<&'a str, &'a Element>,
        parent_of: &mut HashMap<&'a str, Option<&'a str>>,
    ) {
        by_id.insert(e.id.as_str(), e);
        parent_of.insert(e.id.as_str(), parent);
        for c in &e.children {
            index(c, Some(e.id.as_str()), by_id, parent_of);
        }
    }
    let mut old_by_id = HashMap::new();
    let mut old_parent = HashMap::new();
    for r in &old.elements {
        index(r, None, &mut old_by_id, &mut old_parent);
    }
    let mut new_ids = HashSet::new();
    new.each_element(&mut |e| {
        new_ids.insert(e.id.as_str());
    });

    fn diff_node(
        old_by_id: &std::collections::HashMap<&str, &Element>,
        old_parent: &std::collections::HashMap<&str, Option<&str>>,
        parent: Option<&str>,
        new_el: &Element,
        upserts: &mut Vec<ElementUpsert>,
    ) {
        let unchanged_here = match old_by_id.get(new_el.id.as_str()) {
            Some(old_el) => {
                old_parent.get(new_el.id.as_str()).copied().flatten() == parent
                    && element_self_eq(old_el, new_el)
            }
            None => false,
        };
        if !unchanged_here {
            upserts.push(ElementUpsert {
                parent: parent.map(str::to_string),
                element: new_el.clone(),
            });
            return;
        }
        for c in &new_el.children {
            diff_node(old_by_id, old_parent, Some(new_el.id.as_str()), c, upserts);
        }
    }
    let mut upserts = Vec::new();
    for r in &new.elements {
        diff_node(&old_by_id, &old_parent, None, r, &mut upserts);
    }

    let mut removed = Vec::new();
    old.each_element(&mut |e| {
        if new_ids.contains(e.id.as_str()) {
            return;
        }
        // 只报最顶层：父元素还在新帧里（或自己是根）才列；
        // 父元素也消失时，父的移除已覆盖本元素。
        let topmost = match old_parent.get(e.id.as_str()).copied().flatten() {
            None => true,
            Some(p) => new_ids.contains(p),
        };
        if topmost {
            removed.push(e.id.clone());
        }
    });

    ObservationDelta {
        base_frame: old.frame_id,
        frame_id: new.frame_id,
        upserts,
        removed,
    }
}

impl Observation {
    /// 把增量应用到本帧得到新帧；`base_frame` 与本帧不一致返回 None
    /// （接收方必须丢弃增量、请求完整快照）。只更新元素树与
    /// frame_id——text/image 不在增量覆盖内，沿用基帧的。
    /// 已存在且父节点未变的元素原地替换（保序）；父节点变了的
    /// 先摘除再挂到新父之下（追加在末尾，顺序不保证，见
    /// `ObservationDelta` 的说明）。
    pub fn apply_delta(&self, d: &ObservationDelta) -> Option<Observation> {
        if d.base_frame != self.frame_id {
            return None;
        }
        fn remove_by_id(elems: &mut Vec<Element>, id: &str) -> Option<Element> {
            if let Some(pos) = elems.iter().position(|e| e.id == id) {
                return Some(elems.remove(pos));
            }
            for e in elems.iter_mut() {
                if let Some(x) = remove_by_id(&mut e.children, id) {
                    return Some(x);
                }
            }
            None
        }
        fn parent_of(elems: &[Element], id: &str, cur: Option<&str>) -> Option<Option<String>> {
            for e in elems {
                if e.id == id {
                    return Some(cur.map(str::to_string));
                }
                if let Some(p) = parent_of(&e.children, id, Some(e.id.as_str())) {
                    return Some(p);
                }
            }
            None
        }
        fn replace_in_place(elems: &mut [Element], el: &Element) -> bool {
            for e in elems.iter_mut() {
                if e.id == el.id {
                    *e = el.clone();
                    return true;
                }
                if replace_in_place(&mut e.children, el) {
                    return true;
                }
            }
            false
        }
        fn insert_under(elems: &mut Vec<Element>, parent: Option<&str>, el: Element) -> bool {
            match parent {
                None => {
                    elems.push(el);
                    true
                }
                Some(p) => {
                    fn go(elems: &mut [Element], p: &str, el: Element) -> bool {
                        for e in elems.iter_mut() {
                            if e.id == p {
                                e.children.push(el);
                                return true;
                            }
                            if go(&mut e.children, p, el.clone()) {
                                return true;
                            }
                        }
                        false
                    }
                    go(elems, p, el)
                }
            }
        }

        let mut out = self.clone();
        out.frame_id = d.frame_id;
        for id in &d.removed {
            remove_by_id(&mut out.elements, id);
        }
        for u in &d.upserts {
            let cur_parent = parent_of(&out.elements, &u.element.id, None);
            let same_parent = matches!(&cur_parent, Some(p) if p.as_deref() == u.parent.as_deref());
            if same_parent && replace_in_place(&mut out.elements, &u.element) {
                continue;
            }
            remove_by_id(&mut out.elements, &u.element.id);
            if !insert_under(&mut out.elements, u.parent.as_deref(), u.element.clone()) {
                return None; // 父节点不存在：增量损坏
            }
        }
        Some(out)
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ObserveMode {
    /// 只要文本视图。最省。
    Text,
    /// 元素树（默认）。
    #[default]
    Tree,
    /// 元素树 + 对应帧的图像。
    TreeImage,
}

/// 文本视图的排版。只影响 `text` 字段的渲染，不影响结构化观察与动作校验。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TextLayout {
    /// 元素树，一行一元素、缩进表层级（默认）。
    #[default]
    Tree,
    /// 语义块：标题、表格（逐行 `|` 分列）、对话框/iframe 成块，可操作元素带 id；
    /// 不含几何坐标（iframe/画布除外）。
    Blocks,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ObserveRequest {
    #[serde(default)]
    pub mode: ObserveMode,
    /// token 预算上限，Observer 据此裁剪（深度、元素数量、文本长度）。None 表示由 Observer 默认值决定。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub budget_tokens: Option<u32>,
    /// 提供上次收到的帧号时，Observer 可以返回增量。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub since_frame: Option<FrameId>,
    /// 只返回该元素的子树。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subtree: Option<String>,
    #[serde(default)]
    pub include_offscreen: bool,
    /// 文本视图排版，默认 `tree`。
    #[serde(default, skip_serializing_if = "is_default_layout")]
    pub layout: TextLayout,
}

fn is_default_layout(layout: &TextLayout) -> bool {
    *layout == TextLayout::Tree
}

// ───────────────────────────── Action ─────────────────────────────

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "by", rename_all = "snake_case")]
pub enum Target {
    /// 引用元素。首选。
    Element { id: String },
    /// 裸坐标（逻辑像素）。只在目标不是元素时使用（canvas、像素兜底），且只在**当前帧**有效。
    Point { x: f32, y: f32 },
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MouseButton {
    #[default]
    Left,
    Right,
    Middle,
}

fn one() -> u8 {
    1
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Action {
    Click {
        target: Target,
        #[serde(default)]
        button: MouseButton,
        #[serde(default = "one")]
        count: u8,
    },
    Hover {
        target: Target,
    },
    Type {
        /// None 表示输入到当前焦点元素。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        target: Option<Target>,
        text: String,
        /// 输入后是否按回车。
        #[serde(default)]
        submit: bool,
        /// 先全选再输入，覆盖已有内容（缺省 false：追加）。
        #[serde(default)]
        replace: bool,
    },
    /// 按键或组合键，例如 "Enter"、"Control+A"。
    Key {
        key: String,
    },
    Scroll {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        target: Option<Target>,
        dx: f32,
        dy: f32,
    },
    /// 把已观察到的元素滚入视口（不点击、不输入）。目标必须是元素，
    /// 允许视口外元素；滚动后调用方应重新 observe 再对它操作。
    ScrollIntoView {
        target: Target,
    },
    Select {
        target: Target,
        values: Vec<String>,
    },
    /// 把复选框/单选框/开关设为已选或未选（幂等）：已经是目标状态就不动，
    /// 不像 click 那样来回切换。目标必须是元素。
    SetChecked {
        target: Target,
        checked: bool,
    },
    /// 给文件输入框喂文件（仅 Web）。`files` 是边缘主机上的路径，
    /// 边缘经 CDP `DOM.setFileInputFiles` 设置，页面随后照常收到
    /// input/change 事件、走站点自身的上传逻辑。设计见
    /// docs/EDGE_BROWSER.md §10.1。
    Upload {
        target: Target,
        files: Vec<String>,
    },
    /// 导航到 URL（仅 Web）。受目标的 `allowed_origins` 约束。
    Navigate {
        url: String,
    },
}

impl Action {
    pub fn is_navigation(&self) -> bool {
        matches!(self, Action::Navigate { .. })
    }

    /// 该动作引用的所有目标（用于陈旧检查）。
    pub fn targets(&self) -> Vec<&Target> {
        match self {
            Action::Click { target, .. }
            | Action::Hover { target }
            | Action::ScrollIntoView { target }
            | Action::Select { target, .. }
            | Action::Upload { target, .. }
            | Action::SetChecked { target, .. } => vec![target],
            Action::Type { target, .. } | Action::Scroll { target, .. } => target.iter().collect(),
            Action::Key { .. } | Action::Navigate { .. } => Vec::new(),
        }
    }
}

/// 控制权戳。启用 Lease（V2）后，所有产生输入的动作必须携带。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LeaseStamp {
    pub domain: String,
    pub epoch: u64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ActionRequest {
    pub request_id: String,
    /// 发起方看到的帧。陈旧检查以它为基准。
    pub frame_id: FrameId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lease: Option<LeaseStamp>,
    pub action: Action,
}

/// 批量动作的步数上限。
pub const MAX_BATCH_ACTIONS: usize = 8;

/// 批量动作：按序执行，**每一步都以同一个 `frame_id` 独立走完整的校验**，
/// 遇到第一个非 `Done`（被拒或被中断）立即停止，后续步骤不执行。
/// 页面因前一步变化而使后一步目标过期时，后一步被拒即止，不会盲打。
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct BatchActionRequest {
    pub request_id: String,
    pub frame_id: FrameId,
    pub actions: Vec<Action>,
    /// 整批**全部 Done** 后是否随回包附上页面文本；缺省不附。只回文本，不回截图。
    /// 批量在某一步被拒或被中断时沿用原有行为（附最新状态），本字段不起作用。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observe_after: Option<ObserveAfter>,
}

/// 批量动作全部完成后附带的文本呈现。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ObserveAfter {
    /// 相对请求帧的文本行增量（与 `observe` 的 `mode=text` + `since_frame` 同口径，对不上时回完整文本）。
    Delta,
    /// 完整文本。
    Text,
}

/// 批量动作结果：`results` 为已尝试各步的结果（最后一项可能是 Rejected/Aborted），
/// `skipped` 是因提前停止而未尝试的步数。
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct BatchActionResult {
    pub results: Vec<ActionResult>,
    pub skipped: usize,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "code", rename_all = "snake_case")]
pub enum RejectReason {
    /// 目标自该帧以来已变化或消失，请重新观察。
    StaleFrame {
        current_frame: FrameId,
    },
    UnknownElement {
        id: String,
    },
    /// 元素存在但被禁用或在视口外。
    NotInteractable {
        id: String,
    },
    OutOfBounds,
    /// Upload 的目标元素不是 `<input type="file">`。
    NotFileInput {
        id: String,
    },
    /// Upload 的文件在边缘主机上不存在或不是普通文件。
    FileNotFound {
        path: String,
    },
    /// Upload 的文件超过大小上限（200 MiB）。
    FileTooLarge {
        path: String,
    },
    StaleEpoch {
        current_epoch: u64,
    },
    PermissionDenied,
    /// 需要人确认（高风险动作）。
    NeedsConfirmation,
    PolicyDenied {
        detail: String,
    },
}

/// 动作被外部中断的原因。与 Rejected 的区别：请求本身合法，
/// 是执行过程被打断，可能已部分执行。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AbortReason {
    /// 用户在观看端强制接管（交互模型见 docs/EDGE_BROWSER.md §7.3）
    UserTakeover,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "result", rename_all = "snake_case")]
pub enum ActionResult {
    /// 已执行；`frame_id` 是**动作所基于的校验帧**，不是动作之后的新帧。
    /// 操作与呈现解耦：act 只回答“动作有没有执行”，页面因动作变成什么样
    /// 由调用方按需 `observe` 获得。`since_frame` 使用客户端已应用的最后一个观察帧号。
    Done {
        frame_id: FrameId,
        /// 执行附注（如 Upload 目标是单选文件框、多余文件被截断），
        /// 无附注时缺省、不出现在报文里。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        note: Option<String>,
    },
    Rejected {
        reason: RejectReason,
    },
    /// 执行被中断；调用方应重新观察后再决定下一步。
    Aborted {
        reason: AbortReason,
    },
}

/// 同一个元素只是从禁用变成了可用（角色、名称、位置、其他状态都没变）。
/// 典型场景：同一批里前一步取消勾选，随后要填写的输入框随之启用。
/// 元素还是调用方指的那一个，只是更可用了，这不是“目标变了”；反向（可用变禁用）不放行。
fn became_enabled(before: &Element, after: &Element, tol_px: f32) -> bool {
    if !before.has(ElementState::Disabled) || after.has(ElementState::Disabled) {
        return false;
    }
    let mut before = before.clone();
    before.states.retain(|s| *s != ElementState::Disabled);
    element_unchanged(&before, after, tol_px)
}

/// 校验"引用元素"的动作目标是否仍然有效。
///
/// 规则（默认的 ElementStable 策略）：
/// - 请求的帧就是当前帧：元素必须存在且可交互。
/// - 请求的帧已落后：必须还保留着当时的帧（`seen`），且该元素在当前帧中**没有变化**
///   （见 [`element_unchanged`]）；否则 `StaleFrame`。当时的帧已被淘汰（`seen` 为 None）同样视为过期。
///
/// 这样页面上无关区域在持续变化（时钟、动画）时，动作不会被无谓拒绝；
/// 但目标本身移动、改名、消失时一定被拒绝，不会点错。
pub fn check_element_target(
    requested: FrameId,
    seen: Option<&Observation>,
    current: &Observation,
    id: &str,
    tol_px: f32,
) -> Result<(), RejectReason> {
    check_element(requested, seen, current, id, tol_px, true)
}

/// 校验 `scroll_into_view` 的目标：与 [`check_element_target`] 相同的存在与陈旧规则，
/// 但允许目标在视口外或被禁用——滚动不产生点击等副作用，视口外正是它的用途。
pub fn check_element_scroll_target(
    requested: FrameId,
    seen: Option<&Observation>,
    current: &Observation,
    id: &str,
    tol_px: f32,
) -> Result<(), RejectReason> {
    check_element(requested, seen, current, id, tol_px, false)
}

fn check_element(
    requested: FrameId,
    seen: Option<&Observation>,
    current: &Observation,
    id: &str,
    tol_px: f32,
    need_interactable: bool,
) -> Result<(), RejectReason> {
    let current_frame = current.frame_id;
    let stale = move || RejectReason::StaleFrame { current_frame };
    let unknown = || RejectReason::UnknownElement { id: id.to_string() };
    let not_interactable = || RejectReason::NotInteractable { id: id.to_string() };
    let ok = |el: &Element| !need_interactable || el.is_interactable();

    let now = current.find(id);

    if requested == current.frame_id {
        let el = now.ok_or_else(unknown)?;
        return if ok(el) {
            Ok(())
        } else {
            Err(not_interactable())
        };
    }

    let seen = seen.ok_or_else(stale)?;
    if seen.frame_id != requested {
        return Err(stale());
    }
    let before = seen.find(id).ok_or_else(unknown)?;
    let after = now.ok_or_else(stale)?;
    if !element_unchanged(before, after, tol_px) && !became_enabled(before, after, tol_px) {
        return Err(stale());
    }
    if ok(after) {
        Ok(())
    } else {
        Err(not_interactable())
    }
}

/// 校验"裸坐标"目标：只在当前帧有效，且必须落在参考空间内。
pub fn check_point_target(
    requested: FrameId,
    current: &Observation,
    x: f32,
    y: f32,
) -> Result<(), RejectReason> {
    if requested != current.frame_id {
        return Err(RejectReason::StaleFrame {
            current_frame: current.frame_id,
        });
    }
    if !current.space.contains(x, y) {
        return Err(RejectReason::OutOfBounds);
    }
    Ok(())
}

// ───────────────────────────── 人的标注 ─────────────────────────────

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AnnotationKind {
    Click { x: f32, y: f32 },
    Box { bbox: BBox },
    Stroke { points: Vec<(f32, f32)> },
}

/// 人在 Viewer 上的指点。绑定 `frame_id`，因此即使 AI 看到的帧比人看到的晚，也不会指错位置。
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Annotation {
    pub frame_id: FrameId,
    pub kind: AnnotationKind,
    /// 由 `hit_test_interactive` / `hit_test` 解析出的元素 id（可为空）。
    #[serde(default)]
    pub element_ids: Vec<String>,
    /// 人附带的文字说明。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    pub author: String,
}

// ───────────────────────────── 测试 ─────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use ElementState::*;

    fn space() -> Space {
        Space {
            width: 400.0,
            height: 800.0,
            scale: 2.0,
        }
    }

    fn obs(frame: FrameId, elements: Vec<Element>) -> Observation {
        let mut o = Observation::new(frame, ObservationSource::Cdp, space());
        o.elements = elements;
        o
    }

    fn button(id: &str, name: &str, b: [f32; 4]) -> Element {
        Element::new(id, "button", Origin::A11y)
            .with_name(name)
            .with_bbox(b)
    }

    // ── 坐标 ──

    #[test]
    fn space_roundtrip_within_half_pixel() {
        let s = space();
        for (x, y) in [(0.0, 0.0), (13.37, 99.9), (399.0, 799.0)] {
            let (px, py) = s.to_physical(x, y);
            let (lx, ly) = s.to_logical(px, py);
            assert!((lx - x).abs() < 0.5 && (ly - y).abs() < 0.5);
        }
    }

    #[test]
    fn space_validity() {
        assert!(space().is_valid());
        assert!(!Space {
            width: 0.0,
            ..space()
        }
        .is_valid());
        assert!(!Space {
            scale: f32::NAN,
            ..space()
        }
        .is_valid());
    }

    #[test]
    fn image_pixels_map_to_logical_space() {
        let mut o = obs(1, vec![]);
        // 物理分辨率 800x1600，逻辑 400x800
        o.image = Some(ImageRef {
            uri: "ref://f/1".into(),
            width: 800,
            height: 1600,
        });
        assert_eq!(o.image_to_logical(400.0, 800.0), Some((200.0, 400.0)));
        o.image = None;
        assert_eq!(o.image_to_logical(1.0, 1.0), None);
    }

    #[test]
    fn bbox_is_half_open_and_validated() {
        let b = BBox([10.0, 10.0, 20.0, 20.0]);
        assert!(b.contains(10.0, 10.0));
        assert!(!b.contains(20.0, 20.0));
        assert!(b.is_valid());
        assert!(!BBox([5.0, 5.0, 1.0, 9.0]).is_valid());
        assert!(!BBox([0.0, 0.0, f32::INFINITY, 1.0]).is_valid());
    }

    // ── 命中测试（人的点击 ↔ 元素 id）──

    #[test]
    fn hit_test_prefers_smallest_containing_element() {
        let inner = Element::new("e2", "text", Origin::Dom).with_bbox([20.0, 20.0, 60.0, 40.0]);
        let outer = button("e1", "提交", [0.0, 0.0, 100.0, 60.0]).with_child(inner);
        let o = obs(1, vec![outer]);
        assert_eq!(o.hit_test(30.0, 30.0).unwrap().id, "e2");
        assert_eq!(o.hit_test(90.0, 50.0).unwrap().id, "e1");
        assert!(o.hit_test(300.0, 300.0).is_none());
    }

    #[test]
    fn hit_test_interactive_returns_the_button_not_its_label() {
        let label = Element::new("e2", "text", Origin::Dom).with_bbox([20.0, 20.0, 60.0, 40.0]);
        let o = obs(
            1,
            vec![button("e1", "提交", [0.0, 0.0, 100.0, 60.0]).with_child(label)],
        );
        assert_eq!(o.hit_test_interactive(30.0, 30.0).unwrap().id, "e1");
    }

    #[test]
    fn hit_test_skips_offscreen_and_disabled() {
        let off = button("e1", "a", [0.0, 0.0, 10.0, 10.0]).with_states(&[Offscreen]);
        let dis = button("e2", "b", [20.0, 0.0, 30.0, 10.0]).with_states(&[Disabled]);
        let o = obs(1, vec![off, dis]);
        assert!(o.hit_test(5.0, 5.0).is_none());
        assert!(o.hit_test_interactive(25.0, 5.0).is_none());
    }

    #[test]
    fn overlapping_equal_area_later_element_wins() {
        let a = button("e1", "下层", [0.0, 0.0, 10.0, 10.0]);
        let b = button("e2", "上层", [0.0, 0.0, 10.0, 10.0]);
        assert_eq!(obs(1, vec![a, b]).hit_test(5.0, 5.0).unwrap().id, "e2");
    }

    // ── 脱敏 ──

    #[test]
    fn redact_removes_password_and_sensitive_values_everywhere() {
        let pw = Element::new("e1", "textbox", Origin::A11y)
            .with_value("hunter2")
            .with_bbox([0.0, 0.0, 10.0, 10.0])
            .with_states(&[Password]);
        let otp = Element::new("e2", "textbox", Origin::A11y)
            .with_value("123456")
            .with_bbox([0.0, 20.0, 10.0, 30.0])
            .with_states(&[Sensitive]);
        let user = Element::new("e3", "textbox", Origin::A11y).with_value("alice");
        let form = Element::new("e0", "form", Origin::Dom)
            .with_child(pw)
            .with_child(otp)
            .with_child(user);
        let mut o = obs(1, vec![form]);

        assert_eq!(o.sensitive_regions().len(), 2);
        assert_eq!(o.redact_sensitive(), 2);

        let json = serde_json::to_string(&o).unwrap();
        assert!(!json.contains("hunter2"), "{json}");
        assert!(!json.contains("123456"), "{json}");
        assert!(json.contains("alice"), "普通字段不应被脱敏");
    }

    // ── 不可信标记 ──

    #[test]
    fn observation_is_always_marked_untrusted_on_the_wire() {
        let json = serde_json::to_string(&obs(1, vec![])).unwrap();
        assert!(json.contains(r#""content_trust":"untrusted""#), "{json}");
        // 反序列化缺省字段时同样落在 Untrusted
        let back: Observation = serde_json::from_str(
            r#"{"frame_id":1,"source":"cdp","space":{"width":1.0,"height":1.0,"scale":1.0},"elements":[]}"#,
        )
        .unwrap();
        assert_eq!(back.content_trust, ContentTrust::Untrusted);
    }

    // ── 动作校验 ──

    #[test]
    fn scroll_into_view_and_batch_json_shapes_are_stable() {
        let a: Action = serde_json::from_str(
            r#"{"kind":"scroll_into_view","target":{"by":"element","id":"e9"}}"#,
        )
        .unwrap();
        assert_eq!(
            a,
            Action::ScrollIntoView {
                target: Target::Element { id: "e9".into() }
            }
        );
        assert_eq!(a.targets().len(), 1);
        let req: BatchActionRequest = serde_json::from_str(
            r#"{"request_id":"b","frame_id":3,"actions":[{"kind":"key","key":"Tab"}]}"#,
        )
        .unwrap();
        assert_eq!(req.actions.len(), 1);
        assert_eq!(req.observe_after, None);
        let req: BatchActionRequest = serde_json::from_str(
            r#"{"request_id":"b","frame_id":3,"actions":[{"kind":"key","key":"Tab"}],"observe_after":"delta"}"#,
        )
        .unwrap();
        assert_eq!(req.observe_after, Some(ObserveAfter::Delta));
        assert!(serde_json::from_str::<BatchActionRequest>(
            r#"{"request_id":"b","frame_id":3,"actions":[],"observe_after":"image"}"#
        )
        .is_err());
        let res = BatchActionResult {
            results: vec![ActionResult::Done {
                frame_id: 3,
                note: None,
            }],
            skipped: 2,
        };
        assert_eq!(
            serde_json::to_string(&res).unwrap(),
            r#"{"results":[{"result":"done","frame_id":3}],"skipped":2}"#
        );
    }

    #[test]
    fn scroll_target_accepts_offscreen_but_still_rejects_unknown_and_stale() {
        let off = |frame, y| {
            obs(
                frame,
                vec![button("e1", "far", [0.0, y, 10.0, y + 10.0]).with_states(&[Offscreen])],
            )
        };
        let cur = off(5, 2000.0);
        // 视口外元素对 click 不可交互，但可以作为滚动目标
        assert!(check_element_target(5, None, &cur, "e1", 2.0).is_err());
        assert!(check_element_scroll_target(5, None, &cur, "e1", 2.0).is_ok());
        assert!(matches!(
            check_element_scroll_target(5, None, &cur, "e9", 2.0),
            Err(RejectReason::UnknownElement { .. })
        ));
        // 旧帧：目标移动过就是过期，不能滚向过期位置的元素
        let seen = off(3, 2000.0);
        assert!(check_element_scroll_target(3, Some(&seen), &cur, "e1", 2.0).is_ok());
        let moved = off(5, 2600.0);
        assert!(matches!(
            check_element_scroll_target(3, Some(&seen), &moved, "e1", 2.0),
            Err(RejectReason::StaleFrame { .. })
        ));
    }

    #[test]
    fn older_frame_accepts_target_that_became_enabled_but_not_the_reverse() {
        let frame = |n, states: &[ElementState]| {
            obs(
                n,
                vec![button("e1", "city", [0.0, 0.0, 10.0, 10.0]).with_states(states)],
            )
        };
        // 禁用 → 可用：同一元素更可用了，放行
        let seen = frame(3, &[Disabled]);
        let cur = frame(5, &[]);
        assert!(check_element_target(3, Some(&seen), &cur, "e1", 2.0).is_ok());
        // 可用 → 禁用：目标不能再用，照旧被拒（不是陈旧就是不可交互）
        let seen = frame(3, &[]);
        let cur = frame(5, &[Disabled]);
        assert!(check_element_target(3, Some(&seen), &cur, "e1", 2.0).is_err());
        // 禁用 → 可用的同时位置也变了：仍是陈旧
        let seen = frame(3, &[Disabled]);
        let moved = obs(5, vec![button("e1", "city", [0.0, 300.0, 10.0, 310.0])]);
        assert!(matches!(
            check_element_target(3, Some(&seen), &moved, "e1", 2.0),
            Err(RejectReason::StaleFrame { .. })
        ));
        // 禁用 → 可用的同时改名：仍是陈旧
        let renamed = obs(5, vec![button("e1", "other", [0.0, 0.0, 10.0, 10.0])]);
        assert!(matches!(
            check_element_target(3, Some(&seen), &renamed, "e1", 2.0),
            Err(RejectReason::StaleFrame { .. })
        ));
    }

    #[test]
    fn same_frame_target_must_exist_and_be_interactable() {
        let cur = obs(
            5,
            vec![
                button("e1", "ok", [0.0, 0.0, 10.0, 10.0]),
                button("e2", "no", [0.0, 20.0, 10.0, 30.0]).with_states(&[Disabled]),
            ],
        );
        assert!(check_element_target(5, None, &cur, "e1", 2.0).is_ok());
        assert!(matches!(
            check_element_target(5, None, &cur, "e9", 2.0),
            Err(RejectReason::UnknownElement { .. })
        ));
        assert!(matches!(
            check_element_target(5, None, &cur, "e2", 2.0),
            Err(RejectReason::NotInteractable { .. })
        ));
    }

    #[test]
    fn older_frame_is_ok_if_target_unchanged() {
        // 无关区域在变（e7 的名字变了），目标 e1 没变 → 允许
        let seen = obs(
            5,
            vec![
                button("e1", "提交", [0.0, 0.0, 10.0, 10.0]),
                Element::new("e7", "text", Origin::Dom).with_name("10:00"),
            ],
        );
        let cur = obs(
            6,
            vec![
                button("e1", "提交", [0.0, 0.0, 10.0, 10.0]),
                Element::new("e7", "text", Origin::Dom).with_name("10:01"),
            ],
        );
        assert!(check_element_target(5, Some(&seen), &cur, "e1", 2.0).is_ok());
    }

    #[test]
    fn older_frame_is_rejected_if_target_moved_renamed_or_gone() {
        let seen = obs(5, vec![button("e1", "提交", [0.0, 0.0, 10.0, 10.0])]);
        let moved = obs(6, vec![button("e1", "提交", [0.0, 50.0, 10.0, 60.0])]);
        let renamed = obs(6, vec![button("e1", "取消", [0.0, 0.0, 10.0, 10.0])]);
        let gone = obs(6, vec![]);
        for cur in [moved, renamed, gone] {
            assert!(matches!(
                check_element_target(5, Some(&seen), &cur, "e1", 2.0),
                Err(RejectReason::StaleFrame { current_frame: 6 })
            ));
        }
    }

    #[test]
    fn older_frame_that_was_evicted_is_stale() {
        let cur = obs(9, vec![button("e1", "ok", [0.0, 0.0, 10.0, 10.0])]);
        assert!(matches!(
            check_element_target(3, None, &cur, "e1", 2.0),
            Err(RejectReason::StaleFrame { .. })
        ));
        // 给错了帧也一样
        let wrong = obs(4, vec![button("e1", "ok", [0.0, 0.0, 10.0, 10.0])]);
        assert!(matches!(
            check_element_target(3, Some(&wrong), &cur, "e1", 2.0),
            Err(RejectReason::StaleFrame { .. })
        ));
    }

    #[test]
    fn focus_changes_do_not_make_a_target_stale() {
        let seen = obs(5, vec![button("e1", "ok", [0.0, 0.0, 10.0, 10.0])]);
        let cur = obs(
            6,
            vec![button("e1", "ok", [0.0, 0.0, 10.0, 10.0]).with_states(&[Focused])],
        );
        assert!(check_element_target(5, Some(&seen), &cur, "e1", 2.0).is_ok());
    }

    #[test]
    fn small_layout_jitter_within_tolerance_is_not_stale() {
        let seen = obs(5, vec![button("e1", "ok", [0.0, 0.0, 10.0, 10.0])]);
        let cur = obs(6, vec![button("e1", "ok", [1.0, 1.0, 11.0, 11.0])]);
        assert!(check_element_target(5, Some(&seen), &cur, "e1", 2.0).is_ok());
        assert!(check_element_target(5, Some(&seen), &cur, "e1", 0.5).is_err());
    }

    #[test]
    fn point_targets_are_only_valid_on_the_current_frame_and_in_bounds() {
        let cur = obs(5, vec![]);
        assert!(check_point_target(5, &cur, 10.0, 10.0).is_ok());
        assert!(matches!(
            check_point_target(4, &cur, 10.0, 10.0),
            Err(RejectReason::StaleFrame { current_frame: 5 })
        ));
        assert_eq!(
            check_point_target(5, &cur, 500.0, 10.0),
            Err(RejectReason::OutOfBounds)
        );
    }

    // ── 线上格式稳定 ──

    #[test]
    fn action_wire_format_is_stable() {
        let a = Action::Click {
            target: Target::Element { id: "e12".into() },
            button: MouseButton::Left,
            count: 1,
        };
        assert_eq!(
            serde_json::to_string(&a).unwrap(),
            r#"{"kind":"click","target":{"by":"element","id":"e12"},"button":"left","count":1}"#
        );
        // 缺省字段
        let b: Action =
            serde_json::from_str(r#"{"kind":"click","target":{"by":"point","x":1.0,"y":2.0}}"#)
                .unwrap();
        assert_eq!(
            b,
            Action::Click {
                target: Target::Point { x: 1.0, y: 2.0 },
                button: MouseButton::Left,
                count: 1
            }
        );
    }

    #[test]
    fn action_result_and_reject_reason_wire_format() {
        let r = ActionResult::Rejected {
            reason: RejectReason::StaleFrame { current_frame: 7 },
        };
        assert_eq!(
            serde_json::to_string(&r).unwrap(),
            r#"{"result":"rejected","reason":{"code":"stale_frame","current_frame":7}}"#
        );
    }

    #[test]
    fn upload_action_and_reject_codes_wire_format() {
        let a = Action::Upload {
            target: Target::Element { id: "e9".into() },
            files: vec!["/tmp/a.pdf".into()],
        };
        assert_eq!(
            serde_json::to_string(&a).unwrap(),
            r#"{"kind":"upload","target":{"by":"element","id":"e9"},"files":["/tmp/a.pdf"]}"#
        );
        // Upload 的目标参与陈旧校验（引用元素）
        assert_eq!(a.targets().len(), 1);
        let r = ActionResult::Rejected {
            reason: RejectReason::NotFileInput { id: "e9".into() },
        };
        assert_eq!(
            serde_json::to_string(&r).unwrap(),
            r#"{"result":"rejected","reason":{"code":"not_file_input","id":"e9"}}"#
        );
        let r = ActionResult::Rejected {
            reason: RejectReason::FileNotFound {
                path: "/nope".into(),
            },
        };
        assert_eq!(
            serde_json::to_string(&r).unwrap(),
            r#"{"result":"rejected","reason":{"code":"file_not_found","path":"/nope"}}"#
        );
        // Done 无附注时 note 不出现（旧报文兼容）；有附注时带出
        let d = ActionResult::Done {
            frame_id: 3,
            note: None,
        };
        assert_eq!(
            serde_json::to_string(&d).unwrap(),
            r#"{"result":"done","frame_id":3}"#
        );
        let d2: ActionResult = serde_json::from_str(r#"{"result":"done","frame_id":3}"#).unwrap();
        assert_eq!(d, d2);
    }

    #[test]
    fn action_targets_lists_every_referenced_element() {
        let t = Action::Type {
            target: Some(Target::Element { id: "e1".into() }),
            text: "x".into(),
            submit: false,
            replace: false,
        };
        assert_eq!(t.targets().len(), 1);
        assert!(Action::Key {
            key: "Enter".into()
        }
        .targets()
        .is_empty());
        assert!(Action::Navigate {
            url: "https://a".into()
        }
        .is_navigation());
    }

    #[test]
    fn observe_request_defaults() {
        let r: ObserveRequest = serde_json::from_str("{}").unwrap();
        assert_eq!(r.mode, ObserveMode::Tree);
        assert!(!r.include_offscreen);
    }

    // ── 待实现（由实现者在 P1 完成，去掉 ignore）──

    /// diff(old, new) 得到的 delta 应用到 old 上必须等于 new；base_frame 不匹配时 apply 必须失败。
    #[test]
    fn delta_roundtrip_and_base_frame_mismatch() {
        use super::{diff_observations, ObservationSource, Space};
        fn el(id: &str, role: &str, name: &str) -> Element {
            Element::new(id, role, Origin::A11y).with_name(name)
        }
        // 旧帧：根 e1 下挂按钮 e2、输入框 e3（值 a）、文本 e4
        let mut root = el("e1", "generic", "页");
        root.children = vec![
            el("e2", "button", "提交"),
            el("e3", "textbox", "用户名").with_value("a"),
            el("e4", "text", "页脚"),
        ];
        let space = Space {
            width: 800.0,
            height: 600.0,
            scale: 1.0,
        };
        let mut old = Observation::new(1, ObservationSource::Cdp, space);
        old.elements = vec![root];
        // 新帧：e3 的值变了、e4 删除、e1 末尾新增链接 e5
        let mut root2 = el("e1", "generic", "页");
        root2.children = vec![
            el("e2", "button", "提交"),
            el("e3", "textbox", "用户名").with_value("ab"),
            el("e5", "link", "帮助"),
        ];
        let mut new = Observation::new(2, ObservationSource::Cdp, space);
        new.elements = vec![root2];

        let d = diff_observations(&old, &new);
        assert_eq!(d.base_frame, 1);
        assert_eq!(d.frame_id, 2);
        // 变化点只有 e3（值）与新增 e5——祖先 e1 自身没变，不打包整树
        let up_ids: Vec<&str> = d.upserts.iter().map(|u| u.element.id.as_str()).collect();
        assert_eq!(up_ids, ["e3", "e5"]);
        assert!(d.upserts.iter().all(|u| u.parent.as_deref() == Some("e1")));
        assert_eq!(d.removed, ["e4"]);

        let applied = old.apply_delta(&d).expect("base 匹配应可应用");
        assert_eq!(applied, new);

        // base_frame 不匹配 → None
        let mut bad = d.clone();
        bad.base_frame = 99;
        assert!(old.apply_delta(&bad).is_none());

        // 无变化 → 空增量，应用后等于新帧（帧号推进）
        let d2 = diff_observations(&new, &new);
        assert!(d2.upserts.is_empty() && d2.removed.is_empty());
    }

    /// 元素 id 跟踪：连续两帧中同一个 DOM 节点（backendNodeId）应得到同一个 element id；
    /// 节点被销毁后其 id 不得复用给另一个节点。
    #[test]
    #[ignore = "P1：实现元素 id 跟踪"]
    fn element_ids_are_stable_across_frames_and_never_reused() {
        todo!()
    }
}
