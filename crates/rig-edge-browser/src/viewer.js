
const cv = document.getElementById('cv'), ctx = cv.getContext('2d');
const vid = document.getElementById('vid');
const ime = document.getElementById('ime');
const promptBox = document.getElementById('prompt');
/* 输入坐标使用边缘实际应用的 CSS 视口。 */
let cssViewportW = 0, cssViewportH = 0;
const rtcAudio = new Audio();
/* 鉴权由 auth.js 引导（令牌不进 URL）：fch 带 access 头，WS 首帧认证 */
const fch = (u, o) => window.__rigAuth.fetch(u, o);
const ws = window.__rigAuth.openWs();
let avWanted = false, avWait = 0;
let hlsRequested = false, hlsPlaying = false;
ws.binaryType = 'arraybuffer';
let frameSeq = 0;
ws.onmessage = (ev) => {
  if (typeof ev.data !== 'string') {
    /* 二进制帧：8 字节头（宽/高，u32 小端）+ JPEG 原始字节。
       序号守卫：解码慢的旧帧不得覆盖已画的新帧 */
    const dv = new DataView(ev.data);
    const w = dv.getUint32(0, true), h = dv.getUint32(4, true);
    const seq = ++frameSeq;
    const tArr = performance.now();
    /* F-09 观看端段：帧到达→解码上屏耗时（window.__rigPaint，
       探针/诊断读；与边缘 /health 的分段计量互补） */
    window.__rigPaint = window.__rigPaint || { n: 0, lastMs: 0, avgMs: 0 };
    createImageBitmap(new Blob([new Uint8Array(ev.data, 8)], {type: 'image/jpeg'})).then(bmp => {
      /* F-06：过期帧也要 close（此前直接 return 泄漏 bitmap）；
         canvas 尺寸不变不重设（赋值即清画布+重分配，按帧做是浪费） */
      if (seq !== frameSeq) { bmp.close(); return; }
      if (domOn) {
        /* DOM 模式边缘已停发二进制帧；偶有迟到帧直接丢弃 */
      } else {
        if (cv.width !== w || cv.height !== h) { cv.width = w; cv.height = h; }
        ctx.drawImage(bmp, 0, 0);
        if (fbScrollDy) fbScrollReset(); /* F-07：真帧到达，滚动预览归位 */
      }
      bmp.close();
      const rp = window.__rigPaint;
      rp.lastMs = performance.now() - tArr;
      rp.avgMs = rp.n ? rp.avgMs * 0.9 + rp.lastMs * 0.1 : rp.lastMs;
      rp.n++;
    }).catch(() => {});
    return;
  }
  const m = JSON.parse(ev.data);
  if (m.type === 'takeover-prompt') { promptBox.style.display = 'block'; }
  if (m.type === 'exec' && m.state === 'idle') { promptBox.style.display = 'none'; }
  if (m.type === 'print-ready') {
    fch('/print.pdf').then(r => r.blob()).then(b => {
      const a = document.createElement('a');
      a.href = URL.createObjectURL(b); a.download = 'page.pdf'; a.click();
      setTimeout(() => URL.revokeObjectURL(a.href), 4000);
    }).catch(() => {});
  }
  if (m.type === 'clipboard-text') { deliverLocalText(m.text || ''); }
  if (m.type === 'clipboard-image') { deliverLocalImage(m.data || ''); }
  if (m.type === 'image-at') { onImageAt(m.data || ''); }
  if (m.type === 'hello' || (m.type === 'viewport' && m.request === resizeRequest)) {
    applyViewport(m);
  }
  if (m.type === 'mode-summary') applyModeSummary(m);
  if (m.type === 'rtc-offer' && pc) acceptOffer(pc, m.sdp);
  if (m.type === 'rtc-state' && m.state === 'disconnected' && pc) suspendRtc();
  if (m.type === 'dom-snapshot' || m.type === 'dom-delta' || m.type === 'dom-state' || m.type === 'dom-canvas' || m.type === 'dom-region' || m.type === 'dom-chunk') {
    __domStats.ws++; domFeed(m);
  }
  if (m.type === 'ghost') { showGhost(m); if (tookOver) { tookOver = false; toast('AI has resumed control'); } }
  if (m.type === 'ghost-off') hideGhost();
  /* T01：边缘 DC 待发队列丢弃旧代次后请求重同步，观看端回发 dom-resync */
  if (m.type === 'dom-resync-needed') send({type:'dom-resync'});
  if (m.type === 'chrome-state') applyChromeState(m);
};
/* DOM 消息统一入口：WS 文本与 rig-dom 数据通道两条腿共用
   （传输口径：WebRTC 优先、WS 回落；seq 协议与传输无关） */
const __domStats = { ws: 0, dc: 0 };
window.__domStats = __domStats;
/* 诊断句柄（探针/排障用）：传输计数与手动停 RTC 验回落 */
window.__rig = { domStats: __domStats, rtcStop: () => rtcStop() };
/* DC 分片重组：边缘对超过浏览器 SCTP 单条上限的消息（真实页面
   快照可达 1.6MB）发 dom-chunk 帧序列，拼回整条再走原处理；
   WS 不分片，同一入口无害 */
const chunkBuf = new Map();
/* D03 有界：残缺包 10s 过期回收、最多 16 个在途重组、总量 16MB；
   跨 RTC 会话换代时 clear（见 rtcStop）。超限/过期一律丢弃并
   请求全量快照，不拿半条拼。序号/数量/类型先校验，不信任远端。 */
const CHUNK_MAX_N = 4096, CHUNK_MAX_ENTRIES = 16;
const CHUNK_MAX_BYTES = 16 * 1024 * 1024, CHUNK_TTL_MS = 10000;
let chunkBytes = 0;
function chunkSweep(now) {
  let expired = false;
  for (const [id, b] of chunkBuf) {
    if (now - b.t > CHUNK_TTL_MS) {
      chunkBytes -= b.bytes; chunkBuf.delete(id); expired = true;
    }
  }
  if (expired && domOn) send({type:'dom-resync'});
}
function domFeed(m) {
  if (!domOn) return;
  if (m.type === 'dom-snapshot') regionEpochCheck(m.url);
  if (m.type !== 'dom-chunk') { handleDomMessage(m); return; }
  const now = Date.now();
  chunkSweep(now);
  const n = m.n, i = m.i;
  if (!Number.isInteger(n) || n < 1 || n > CHUNK_MAX_N) return;
  if (!Number.isInteger(i) || i < 0 || i >= n) return;
  if (typeof m.data !== 'string') return;
  let b = chunkBuf.get(m.id);
  if (!b) {
    if (chunkBuf.size >= CHUNK_MAX_ENTRIES) {
      // 满了：丢最旧一个（Map 按插入序），并请求全量快照
      const oldest = chunkBuf.keys().next().value;
      const ob = chunkBuf.get(oldest);
      chunkBytes -= ob.bytes; chunkBuf.delete(oldest);
      if (domOn) send({type:'dom-resync'});
    }
    b = { parts: new Array(n), got: 0, bytes: 0, t: now };
    chunkBuf.set(m.id, b);
  }
  if (b.parts[i] === undefined) {
    b.parts[i] = m.data; b.got++;
    b.bytes += m.data.length; chunkBytes += m.data.length;
    if (chunkBytes > CHUNK_MAX_BYTES) {
      chunkBuf.clear(); chunkBytes = 0;
      if (domOn) send({type:'dom-resync'});
      return;
    }
  }
  if (b.got === n) {
    chunkBytes -= b.bytes; chunkBuf.delete(m.id);
    try { handleDomMessage(JSON.parse(b.parts.join(''))); } catch (e) {}
  }
}
function handleDomMessage(m) {
  if (m.type === 'dom-snapshot' && domOn) applySnapshot(m);
  if (m.type === 'dom-delta' && domOn) applyDelta(m);
  if (m.type === 'dom-canvas' && domOn) applyCanvas(m);
  if (m.type === 'dom-region' && domOn) applyRegion(m);
  if (m.type === 'dom-state' && m.state === 'error' && domOn) domFallback(m.reason || 'edge error');
}
/* 区域子流帧：跨源 iframe / video 占位块贴图（带外子流，无 seq；
   占位块不在当前镜像里就只进缓存，等下次建节点时贴上） */
function applyRegion(m) {
  const url = 'data:image/jpeg;base64,' + m.d;
  regionPut(m.i, url);
  const doc = mir.contentDocument;
  if (!doc) return;
  const host = doc.querySelector('[data-rig-region="' + m.i + '"]');
  if (!host) return;
  const img = host.querySelector('img[data-rig-region-img]');
  if (img) { img.src = url; img.style.display = 'block'; }
}
/* canvas 子流帧：把占位块换成图片、后续帧只换 src（带外子流，
   无 seq；节点不在当前镜像里就忽略） */
function applyCanvas(m) {
  const host = mirMap.get(m.i);
  if (!host || !host.ownerDocument) return;
  let img = host.querySelector ? host.querySelector('img[data-rig-canvas-img]') : null;
  if (!img) {
    host.textContent = '';
    img = host.ownerDocument.createElement('img');
    img.setAttribute('data-rig-canvas-img', '1');
    img.setAttribute('style', 'width:100%;height:100%;display:block');
    host.appendChild(img);
  }
  img.src = m.d;
}
function send(o) { if (ws.readyState === 1) ws.send(JSON.stringify(o)); }
/* 强制接管后 AI 若恢复操作（收到 ghost 指针），给明确提示 */
let tookOver = false;
function toast(text) {
  const el = document.createElement('div');
  el.textContent = text;
  el.style.cssText = 'position:fixed;top:56px;left:50%;transform:translateX(-50%);z-index:9000;padding:6px 14px;border-radius:6px;background:#c62828;color:#fff;font:13px system-ui,sans-serif;pointer-events:none';
  document.body.appendChild(el);
  setTimeout(() => el.remove(), 3000);
}
document.getElementById('pYes').onclick = () => { promptBox.style.display = 'none'; tookOver = true; send({type:'takeover', yes:true}); ime.focus(); };
document.getElementById('pNo').onclick = () => { promptBox.style.display = 'none'; send({type:'takeover', yes:false}); ime.focus(); };
function frameRect() {
  return (hlsPlaying || rtcOn ? vid : cv).getBoundingClientRect();
}
function toFrame(e) {
  const r = frameRect();
  return { x: (e.clientX - r.left) * cssViewportW / Math.max(1, r.width),
           y: (e.clientY - r.top) * cssViewportH / Math.max(1, r.height) };
}
/* ── F-07 本地即时反馈（仅视频/画布模式；DOM 模式原生即时）──
   点击涟漪、拖选框、滚动预览都在本地先行呈现，页面结果态
   仍以远端帧为准——不伪造成功，只消除「点了没反应」的空窗 */
const fbRing = document.getElementById('fbRing');
const fbRect = document.getElementById('fbRect');
let fbDrag = null, fbRingTimer = 0;
function fbClick(x, y) {
  fbRing.style.left = x + 'px'; fbRing.style.top = y + 'px';
  fbRing.style.display = 'block';
  fbRing.style.transform = 'scale(.4)'; fbRing.style.opacity = '1';
  requestAnimationFrame(() => { fbRing.style.transition = 'transform 140ms ease-out, opacity 140ms'; fbRing.style.transform = 'scale(1.15)'; fbRing.style.opacity = '.25'; });
  clearTimeout(fbRingTimer);
  fbRingTimer = setTimeout(() => { fbRing.style.display = 'none'; fbRing.style.transition = ''; }, 160);
}
function fbDragMove(x, y) {
  if (!fbDrag) return;
  const dx = x - fbDrag.x, dy = y - fbDrag.y;
  if (Math.abs(dx) < 4 && Math.abs(dy) < 4) return;
  fbRect.style.display = 'block';
  fbRect.style.left = Math.min(fbDrag.x, x) + 'px';
  fbRect.style.top = Math.min(fbDrag.y, y) + 'px';
  fbRect.style.width = Math.abs(dx) + 'px';
  fbRect.style.height = Math.abs(dy) + 'px';
}
let fbScrollDy = 0, fbScrollTimer = 0;
function fbScrollReset() {
  fbScrollDy = 0;
  cv.style.transform = ''; vid.style.transform = '';
}
function fbScroll(dy) {
  fbScrollDy = Math.max(-2400, Math.min(2400, fbScrollDy + dy));
  const t = 'translateY(' + (-fbScrollDy) + 'px)';
  /* 只预览画布（WS 帧）：每张真帧到达即归位（见 onmessage）。RTC/HLS 的
     <video> 是连续流、没有「真帧到达」可归位的时机，预览位移会叠在流自身的
     滚动之上——连续滚动时画面被双倍位移、停手 180ms 后又整体弹回 */
  if (cv.style.display !== 'none') cv.style.transform = t;
  clearTimeout(fbScrollTimer);
  fbScrollTimer = setTimeout(fbScrollReset, 180); /* 帧流追上后归位 */
}
const mouseMasks = [1, 4, 2, 8, 16];
let heldButtons = 0, lastMove = 0, pendingMove = null, moveTimer = null;

function flushMove() {
  clearTimeout(moveTimer);
  moveTimer = null;
  if (!pendingMove) return;
  const move = pendingMove;
  pendingMove = null;
  lastMove = performance.now();
  send(move);
}
function movePointer(e, point, dom) {
  pendingMove = { type:'mouse', action:'move', dom, buttons:heldButtons, modifiers:mods(e), ...point };
  if (performance.now() - lastMove >= 24) flushMove();
  else if (!moveTimer) moveTimer = setTimeout(flushMove, 24);
}
function mouseButton(e, action, point, dom) {
  const mask = mouseMasks[e.button];
  if (!mask || (action === 'up' && !(heldButtons & mask))) return;
  flushMove();
  heldButtons = action === 'down' ? heldButtons | mask : heldButtons & ~mask;
  send({type:'mouse', action, dom, button:e.button, buttons:heldButtons,
    count:e.detail, modifiers:mods(e), ...point});
}
function resetInput() {
  clearTimeout(moveTimer);
  moveTimer = pendingMove = null;
  heldButtons = 0;
  heldKeys.clear();
  fbDrag = null;
  fbRect.style.display = 'none';
  send({type:'input-reset'});
}
function overFrame({x, y}) {
  return x >= 0 && y >= 0 && x < cssViewportW && y < cssViewportH;
}
function mirrorPoint(e) {
  const r = mir.getBoundingClientRect();
  const scale = r.width / Math.max(1, mirVW) || 1;
  return { x:(e.clientX - r.left) / scale, y:(e.clientY - r.top) / scale };
}
function mirrorRootPoint(doc, x, y) {
  for (let frame = doc.defaultView?.frameElement; frame && frame !== mir; frame = frame.ownerDocument.defaultView?.frameElement) {
    const r = frame.getBoundingClientRect();
    x = r.left + (x + frame.clientLeft) * r.width / Math.max(1, frame.offsetWidth);
    y = r.top + (y + frame.clientTop) * r.height / Math.max(1, frame.offsetHeight);
  }
  return {x, y};
}
function mirrorContext(doc, x, y, target, keyboard = false) {
  const point = mirrorRootPoint(doc, x, y);
  const r = mir.getBoundingClientRect();
  const scale = r.width / Math.max(1, mirVW) || 1;
  const node = target?.closest?.('[data-rig-region]') || target;
  const id = node?.__rigId;
  const hit = id === undefined ? null : {kind:'node', i:id};
  openCtx(r.left + point.x * scale, r.top + point.y * scale, hit, doc.activeElement, keyboard);
}
function wheelPixels(e) {
  if (e.deltaMode === 2) return {dx:e.deltaX * cssViewportW, dy:e.deltaY * cssViewportH};
  const unit = e.deltaMode === 1 ? 16 : 1;
  return {dx:e.deltaX * unit, dy:e.deltaY * unit};
}
ime.addEventListener('mousedown', e => {
  const point = toFrame(e);
  if (!overFrame(point)) return;
  e.preventDefault();
  ime.focus({preventScroll:true});
  if (e.button === 0) {
    fbClick(e.clientX, e.clientY);
    fbDrag = {x:e.clientX, y:e.clientY};
  }
  mouseButton(e, 'down', point, false);
});
/* 抬起可落在舞台之外。仍把它送回按下的页面，避免远端卡在拖动。 */
document.addEventListener('mouseup', e => {
  mouseButton(e, 'up', domOn ? mirrorPoint(e) : toFrame(e), domOn);
  if (e.button === 0 && fbDrag) {
    fbDrag = null;
    setTimeout(() => { if (!fbDrag) fbRect.style.display = 'none'; }, 120);
  }
}, true);
ime.addEventListener('mousemove', e => {
  const point = toFrame(e);
  if (heldButtons) fbDragMove(e.clientX, e.clientY);
  movePointer(e, point, false);
});
ime.addEventListener('mouseleave', e => {
  if (!heldButtons) movePointer(e, {x:-1, y:-1}, false);
});
ime.addEventListener('wheel', e => {
  e.preventDefault();
  const point = toFrame(e);
  if (!overFrame(point)) return;
  const delta = wheelPixels(e);
  fbScroll(delta.dy);
  send({type:'wheel', ...delta, modifiers:mods(e), ...point});
}, {passive:false});
ime.addEventListener('contextmenu', e => {
  e.preventDefault();
  const point = toFrame(e);
  if (overFrame(point)) openCtx(e.clientX, e.clientY, point);
});
window.addEventListener('blur', () => {
  setTimeout(() => { if (!document.hasFocus()) resetInput(); }, 0);
});
document.addEventListener('visibilitychange', () => {
  if (document.hidden) resetInput();
});
/* 双击归远端页面：旧实现把输入层 dblclick 绑成观看页全屏
   切换，用户在远端页面里双击（选词/视频全屏）全被本地劫持
   （LIU 2026-11-20 指出）。现只挡默认行为；双击语义靠
   mousedown/up 的 count（MouseEvent.detail）透传到远端，
   远端视频双击就在远端页内全屏。视频元素本体也挡一道，
   防输入层放行时浏览器对 <video> 的原生双击全屏 */
ime.addEventListener('dblclick', e => { e.preventDefault(); });
ime.addEventListener('auxclick', e => { e.preventDefault(); });
vid.addEventListener('dblclick', e => { e.preventDefault(); });
function mods(e) { return (e.altKey ? 1 : 0) | (e.ctrlKey ? 2 : 0) | (e.metaKey ? 4 : 0) | (e.shiftKey ? 8 : 0); }
const heldKeys = new Set();
function keyId(e) { return (e.code || e.key) + ':' + e.location; }
function clipboardKey(e) {
  if (e.altKey || e.getModifierState('AltGraph')) return '';
  const primary = e.ctrlKey || e.metaKey;
  const key = e.key.toLowerCase();
  if (primary && key === 'c') return 'copy';
  if (primary && key === 'x') return 'cut';
  if (primary && key === 'v') return 'paste';
  if (e.key === 'Insert') {
    if (e.shiftKey && !primary) return 'paste';
    if (e.ctrlKey && !e.shiftKey) return 'copy';
  }
  if (e.key === 'Delete' && e.shiftKey && !primary) return 'cut';
  return '';
}
function sendViewerKey(e, action, dom) {
  send({type:'key', action, dom, key:e.key, code:e.code, modifiers:mods(e),
    repeat:e.repeat, location:e.location, altGraph:e.getModifierState('AltGraph')});
}
function keyDown(e, dom, editable) {
  if (e.isComposing || e.key === 'Process' || e.key === 'Dead' || e.keyCode === 229) return;
  const clipboard = clipboardKey(e);
  if (clipboard === 'paste' || (dom && clipboard)) return;
  if (!clipboard && !editable) e.preventDefault();
  if (!dom && !clipboard) ime.value = '';
  heldKeys.add(keyId(e));
  sendViewerKey(e, 'down', dom);
  // 保留本地剪贴板的粘贴事件，复制和剪切由边缘执行后读回。
  if (!dom && clipboard && !e.repeat) setTimeout(requestCopy, 160);
}
function keyUp(e, dom) {
  if (heldKeys.delete(keyId(e))) sendViewerKey(e, 'up', dom);
}
ime.addEventListener('keydown', e => keyDown(e, false, false));
document.addEventListener('keyup', e => keyUp(e, domOn), true);
/* 输入法：组字过程留在本地，只把成品整段送过去（边缘按状态路由：
   地址栏编辑时进编辑器，页面焦点时 insertText 直插页面） */
ime.addEventListener('compositionend', e => { if (e.data) send({type:'text', text: e.data}); ime.value = ''; });
/* 粘贴：图片优先（发服务端写远端剪贴板 + 真实 Ctrl+V），否则文本直送 */
ime.addEventListener('paste', e => {
  const cd = e.clipboardData || window.clipboardData;
  for (const it of (cd.items || [])) {
    if (it.type === 'image/png' || it.type === 'image/jpeg') {
      e.preventDefault();
      const blob = it.getAsFile();
      const rd = new FileReader();
      rd.onload = () => send({type:'paste-image', data: String(rd.result).split(',')[1]});
      rd.readAsDataURL(blob);
      return;
    }
  }
  const t = cd.getData('text');
  if (t) { e.preventDefault(); send({type:'text', text: t}); }
});
/* 剪贴板桥（显式模型，借鉴 Go 参考实现）：不做后台同步、不做
   本地暂存补写——那套机制会在用户点击/按键时把待发文本写进本地
   剪贴板，覆盖他刚在外部复制的内容（2026-11-20 实测事故）。
   复制只在用户明确按 Ctrl+C/X 或点菜单「复制」时发生：向边缘发
   clipboard-get，边缘回当前选区（为空回落远端剪贴板），本页拿
   到后 writeText 写本地（HTTPS 安全上下文）；写不进或内容为空
   时弹手动复制框（文本已填好选好）。粘贴靠真实 paste 事件
   （clipboardData 直送），与安全上下文无关。 */
function deliverLocalText(text) {
  if (!text) { flashHint('No text selected'); return; }
  if (navigator.clipboard && navigator.clipboard.writeText) {
    navigator.clipboard.writeText(text)
      .then(() => flashHint('Copied ' + text.length + ' characters'))
      .catch(() => showCopyBox(text));
  } else showCopyBox(text);
}
function requestCopy() {
  send({type:'clipboard-get', ...(domOn ? {dom:true} : {})});
  /* 页面自己复制的图片（canvas/按钮复制流）不在选区里：顺带
     请求读回远端剪贴板图片，有图才回、回了才写本地 */
  if (!domOn) send({type:'clipboard-read-image'});
}
function deliverLocalImage(b64) {
  if (!b64 || !navigator.clipboard || !navigator.clipboard.write || typeof ClipboardItem === 'undefined') return;
  try {
    const bin = atob(b64);
    const bytes = new Uint8Array(bin.length);
    for (let i = 0; i < bin.length; i++) bytes[i] = bin.charCodeAt(i);
    navigator.clipboard.write([new ClipboardItem({'image/png': new Blob([bytes], {type: 'image/png'})})])
      .then(() => flashHint('Image copied'))
      .catch(() => {});
  } catch (e) {}
}
let copyBoxEl = null;
function showCopyBox(text) {
  if (!copyBoxEl) {
    copyBoxEl = document.createElement('div');
    copyBoxEl.id = 'copyBox';
    const tip = document.createElement('div');
    tip.textContent = 'Automatic clipboard write is unavailable. Copy manually (text is selected; press Ctrl+C, then close):';
    tip.style.marginBottom = '6px';
    const ta = document.createElement('textarea');
    ta.rows = 4; ta.style.width = '100%'; ta.readOnly = true;
    const btn = document.createElement('button');
    btn.type = 'button'; btn.textContent = 'Close';
    btn.style.marginTop = '6px';
    btn.onclick = () => { copyBoxEl.style.display = 'none'; ime.focus(); };
    copyBoxEl.append(tip, ta, btn);
    document.body.appendChild(copyBoxEl);
  }
  copyBoxEl.style.display = 'block';
  const ta = copyBoxEl.querySelector('textarea');
  ta.value = text;
  ta.focus(); ta.select();
}
function flashHint(msg) {
  let el = document.getElementById('hintToast');
  if (!el) { el = document.createElement('div'); el.id = 'hintToast'; document.body.appendChild(el); }
  el.textContent = msg;
  el.style.display = 'block';
  clearTimeout(el._t);
  el._t = setTimeout(() => { el.style.display = 'none'; }, 1800);
}
/* 观看端右键菜单（画在本页 DOM，不注入远端页面）：右键本身照常
   转发给远端页面——页面自己的自定义菜单会出现在画面/镜像里；
   本菜单只提供跨边界桥接动作（复制选区/粘贴/复制图片）与导航
   兜底。旧实现把菜单注入远端页面：污染页面、与页面菜单互斥、
   DOM 模式还要靠镜像回传，均已弃。 */
let ctxEl = null, ctxImgData = '', ctxSource = null;
function closeCtx(restoreFocus = false) {
  if (!ctxEl) return;
  ctxEl.remove();
  ctxEl = null;
  if (restoreFocus) {
    if (ctxSource?.isConnected) ctxSource.focus({preventScroll:true});
    else focusPage();
  }
  ctxSource = null;
  document.removeEventListener('pointerdown', ctxOutside, true);
}
function contextMenuKey(e) {
  if (!ctxEl) return false;
  if (e.key === 'Tab') { closeCtx(true); return false; }
  if (!['Escape', 'ArrowDown', 'ArrowUp', 'Home', 'End'].includes(e.key)) return false;
  e.preventDefault();
  e.stopPropagation();
  if (e.key === 'Escape') { closeCtx(true); return true; }
  const items = [...ctxEl.querySelectorAll('button:not(:disabled)')];
  const index = items.indexOf(document.activeElement);
  let next = 0;
  if (e.key === 'End' || (e.key === 'ArrowUp' && index < 0)) next = items.length - 1;
  else if (e.key === 'ArrowDown') next = (index + 1) % items.length;
  else if (e.key === 'ArrowUp') next = (index - 1 + items.length) % items.length;
  items[next]?.focus({preventScroll:true});
  return true;
}
function ctxOutside(e) { if (ctxEl && !ctxEl.contains(e.target)) closeCtx(); }
function sendKeyChord(key, code, mod) {
  const modifiers = mod === undefined ? 2 : mod;
  const base = domOn ? {dom:true} : {};
  send({type:'key', ...base, action:'down', key, code, modifiers});
  send({type:'key', ...base, action:'up', key, code, modifiers});
}
async function readLocalClipText() {
  if (navigator.clipboard && navigator.clipboard.readText) {
    try { return await navigator.clipboard.readText(); } catch (e) {}
  }
  return '';
}
function openCtx(cx, cy, hit, source = document.activeElement, keyboard = false) {
  closeCtx();
  ctxSource = source;
  ctxImgData = '';
  const menu = document.createElement('div');
  menu.id = 'ctxMenu';
  menu.setAttribute('role', 'menu');
  menu.setAttribute('aria-label', 'Page actions');
  const add = (label, fn) => {
    const b = document.createElement('button');
    b.type = 'button'; b.textContent = label;
    b.tabIndex = -1; b.setAttribute('role', 'menuitem');
    b.onclick = () => { closeCtx(true); fn(); };
    menu.appendChild(b);
  };
  add('Copy', () => { if (domOn) copyMirrorSelection(); else requestCopy(); });
  add('Paste', async () => {
    const t = await readLocalClipText();
    if (t) { send({type:'text', ...(domOn ? {dom:true} : {}), text: t}); return; }
    flashHint('Press Ctrl+V to paste');
    ime.focus();
  });
  add('Select all', selectPage);
  add('Auto mode: ' + (modeAuto ? 'on' : 'off'), () => {
    modeAuto = !modeAuto;
    resetAutoMode();
    if (modeAuto) { requestModeSummary(); flashHint('Auto mode on'); }
    else flashHint('Auto mode off');
  });
  /* 导航走 chrome 命令（双模式同一路；旧实现 DOM 模式走 nav
     消息、视频模式发 Alt+←/F5 和弦模拟，口径不一） */
  add('Back', () => chromeCmd('back'));
  add('Forward', () => chromeCmd('forward'));
  add('Reload', () => chromeCmd('reload'));
  menu.style.left = cx + 'px'; menu.style.top = cy + 'px';
  document.body.appendChild(menu);
  const r = menu.getBoundingClientRect();
  menu.style.left = Math.max(4, Math.min(cx, innerWidth - r.width - 4)) + 'px';
  menu.style.top = Math.max(4, Math.min(cy, innerHeight - r.height - 4)) + 'px';
  ctxEl = menu;
  if (keyboard) menu.querySelector('button')?.focus({preventScroll:true});
  document.addEventListener('pointerdown', ctxOutside, true);
  /* 命中图片则异步多一项「复制图片」：视频模式按帧坐标在远端
     页命中测试；DOM 模式按节点 id 定位（镜像布局会漂移，坐标
     命中不可靠）。像素可读才回数据（跨源污染回空、不显示该项） */
  if (hit && hit.kind === 'node') send({type:'image-at-node', dom:true, i: hit.i});
  else if (hit) send({type:'image-at', ...(domOn ? {dom:true} : {}), x: hit.x, y: hit.y});
}
function onImageAt(data) {
  ctxImgData = data || '';
  if (!ctxEl || !data) return;
  if (ctxEl.querySelector('[data-act="copyimg"]')) return;
  const b = document.createElement('button');
  b.type = 'button'; b.textContent = 'Copy image'; b.setAttribute('data-act', 'copyimg');
  b.tabIndex = -1; b.setAttribute('role', 'menuitem');
  b.onclick = () => {
    closeCtx(true);
    if (!(navigator.clipboard && navigator.clipboard.write)) { flashHint('Copying images is not supported in this environment'); return; }
    fetch('data:image/png;base64,' + ctxImgData).then(r => r.blob()).then(blob =>
      navigator.clipboard.write([new ClipboardItem({'image/png': blob})])
        .then(() => flashHint('Image copied')).catch(() => flashHint('Failed to copy the image')));
  };
  ctxEl.appendChild(b);
}
function activeMirrorDocument() {
  let doc = mir.contentDocument;
  while (doc?.activeElement?.tagName === 'IFRAME' && doc.activeElement.contentDocument) {
    doc = doc.activeElement.contentDocument;
  }
  return doc;
}
function selectPage() {
  if (domOn) {
    const doc = activeMirrorDocument();
    const field = doc?.activeElement;
    if (typeof field?.select === 'function') field.select();
    else if (doc?.body) doc.getSelection()?.selectAllChildren(field?.isContentEditable ? field : doc.body);
  }
  sendKeyChord('a', 'KeyA');
}
function mirrorSelText(doc) {
  const a = doc.activeElement;
  if (a && (a.tagName === 'INPUT' || a.tagName === 'TEXTAREA') && typeof a.selectionStart === 'number') {
    return a.value.substring(a.selectionStart, a.selectionEnd);
  }
  return String(doc.getSelection() || '');
}
function copyMirrorSelection() {
  const doc = activeMirrorDocument();
  if (!doc) return;
  const t = mirrorSelText(doc);
  if (t) deliverLocalText(t); else requestCopy();
}

/* 分辨率跟随：把舞台区像素尺寸报给边缘（仅边缘处于「跟随窗口」
   模式时生效）。第五期起头部是文档流内的原生 DOM，舞台区就是
   远端视口的准确占位（不再手工扣条带高） */
const stage = document.getElementById('stage');
let resizeTimer = 0, resizeRequest = 0, resizeSent = null;
function stageSize() {
  const dpr = window.devicePixelRatio || 1;
  return [Math.max(320, Math.round(stage.clientWidth * dpr)),
          Math.max(240, Math.round(stage.clientHeight * dpr))];
}
function reportSize() {
  clearTimeout(resizeTimer);
  if (ws.readyState !== 1) return;
  const [w, h] = stageSize();
  if (resizeSent && w === resizeSent.w && h === resizeSent.h &&
      (resizeSent.applied || performance.now() - resizeSent.at < 3000)) return;
  const report = () => {
    const request = ++resizeRequest;
    resizeSent = {w, h, at: performance.now(), applied: false};
    send({type:'resize', request, w, h});
  };
  if (!resizeSent) report();
  else resizeTimer = setTimeout(report, 900); /* 窗口尺寸稳定后再触发远端重排。 */
}
function applyViewport(m) {
  if (!(m.cssWidth > 0 && m.cssHeight > 0)) return;
  cssViewportW = m.cssWidth; cssViewportH = m.cssHeight;
  if (m.type === 'viewport' && resizeSent) resizeSent.applied = true;
  if (domOn && mirMap.size) {
    mirVW = cssViewportW; mirVH = cssViewportH;
    layoutMirror();
  }
  placeGhost();
}
new ResizeObserver(reportSize).observe(stage);
ws.addEventListener('open', reportSize);
setInterval(() => {
  if (document.visibilityState === 'visible' && document.hasFocus()) reportSize();
}, 3000);

let pc = null, rtcOn = false, rtcSuspended = false, rtcTimer = 0;
let rtcLastTracks = '';
function showDisplay() {
  if (domOn) {
    /* 镜像填满舞台（头部是文档流内的原生工具栏，不占舞台）。
       输入覆盖层必须让出指针事件：它恒覆盖舞台，此前 DOM 模式
       的点击/滚轮全被它截走——镜像自身的转发处理器从未收到
       真实事件。键盘焦点由镜像文档自身承接（点击即聚焦） */
    ime.style.pointerEvents = 'none';
    mir.style.display = 'block';
    mir.style.position = 'absolute';
    layoutMirror();
    vid.style.display = 'none'; cv.style.display = 'none';
    setPill(lastDomStat);
  } else if (hlsPlaying || rtcOn) {
    ime.style.pointerEvents = '';
    vid.style.display = 'block'; cv.style.display = 'none';
    mir.style.display = 'none'; dpill.style.display = 'none';
  } else {
    ime.style.pointerEvents = '';
    vid.style.display = 'none'; cv.style.display = 'block';
    mir.style.display = 'none'; dpill.style.display = 'none';
  }
  document.getElementById('domBtn').classList.toggle('on', domOn);
  placeGhost();
  syncRtcDemand();
  jdSend();
}
function jdSend() {
  const on = !domOn && !hlsPlaying && !rtcOn;
  if (ws.readyState !== 1 || jdSend.last === on) return;
  jdSend.last = on;
  send({type: 'jpeg-demand', on});
}
function syncRtcDemand(start = false) {
  if (!pc) return;
  const video = !domOn && !hlsRequested && !rtcSuspended;
  const audio = avWanted && !hlsRequested;
  const key = video + ':' + audio;
  if (!start && key === rtcLastTracks) return;
  rtcLastTracks = key;
  send({type: start ? 'rtc-start' : 'rtc-demand', video, audio});
}
async function acceptOffer(peer, sdp) {
  try {
    await peer.setRemoteDescription({type:'offer', sdp});
    const answer = await peer.createAnswer();
    await peer.setLocalDescription(answer);
    if (pc === peer) send({type:'rtc-answer', sdp:answer.sdp});
  } catch (e) {
    if (pc === peer) rtcStop();
  }
}
function rtcStop() {
  clearTimeout(rtcTimer);
  const peer = pc;
  pc = null; rtcOn = false; rtcSuspended = false;
  if (peer) peer.close();
  vid.srcObject = null;
  rtcAudio.srcObject = null;
  chunkBuf.clear(); chunkBytes = 0;
  avWait = Date.now();
  send({type:'rtc-stop'});
  showDisplay();
}
function suspendRtc() {
  if (rtcSuspended) return;
  rtcOn = false; rtcSuspended = true;
  avWait = Date.now();
  showDisplay();
  clearTimeout(rtcTimer);
  rtcTimer = setTimeout(rtcStop, 3000);
}
function rtcStart() {
  if (ws.readyState !== 1 || pc || hlsRequested || !window.RTCPeerConnection) return;
  const peer = new RTCPeerConnection();
  pc = peer;
  peer.ondatachannel = (ev) => {
    if (ev.channel.label !== 'rig-dom') return;
    ev.channel.onmessage = (e) => {
      if (pc !== peer || !domOn) return;
      try { __domStats.dc++; domFeed(JSON.parse(e.data)); } catch (err) {}
    };
  };
  peer.ontrack = (ev) => {
    if (pc !== peer) return;
    if (ev.track.kind === 'audio') {
      rtcAudio.srcObject = new MediaStream([ev.track]);
      resumeVid();
    } else {
      vid.muted = true;
      vid.srcObject = new MediaStream([ev.track]);
      vid.play().catch(() => {});
    }
  };
  peer.onicecandidate = (ev) => {
    if (pc === peer && ev.candidate) send({type:'rtc-candidate', candidate:ev.candidate.toJSON()});
  };
  peer.onconnectionstatechange = () => {
    if (pc !== peer) return;
    if (peer.connectionState === 'connected') {
      clearTimeout(rtcTimer);
      rtcOn = true; rtcSuspended = false;
      showDisplay();
    } else if (peer.connectionState === 'disconnected') {
      suspendRtc();
    } else if (peer.connectionState === 'failed' || peer.connectionState === 'closed') {
      rtcStop();
    }
  };
  syncRtcDemand(true);
  showDisplay();
  rtcTimer = setTimeout(rtcStop, 8000);
}
ws.addEventListener('open', rtcStart);

/* HLS 合流只做视频面的兜底，等到开始播放后才离开 JPEG。 */
function startHls() {
  hlsRequested = true;
  rtcStop();
  const offered = window.__rigHls || ['hevc', 'h264'];
  const hevcOk = offered.includes('hevc') && window.MediaSource && MediaSource.isTypeSupported('video/mp4; codecs="hvc1.1.6.L93.B0, mp4a.40.2"');
  vid.muted = false;
  vid.src = '/stream/' + window.__rigAuth.streamKey() + (hevcOk ? '/hevc/index.m3u8' : '/h264/index.m3u8');
  vid.play().catch(() => {});
}
function stopHls() {
  if (!hlsRequested) return;
  hlsRequested = hlsPlaying = false;
  vid.pause(); vid.removeAttribute('src'); vid.load();
  avWait = Date.now();
  showDisplay();
}
vid.addEventListener('playing', () => {
  if (hlsRequested) { hlsPlaying = true; showDisplay(); }
});
vid.addEventListener('error', stopHls);

async function pollAv() {
  try {
    const s = await (await fch('/channels')).json();
    if (Array.isArray(s.hls_renditions)) window.__rigHls = s.hls_renditions;
    if (avWanted !== !!s.av_wanted) {
      avWanted = !!s.av_wanted;
      avWait = Date.now();
      syncRtcDemand();
      if (avWanted) resumeVid();
      else {
        rtcAudio.pause();
        if (hlsRequested) { stopHls(); rtcStart(); }
      }
    }
    if (avWanted && !domOn && !rtcOn && !pc && !hlsRequested && Date.now() - avWait > 5000) {
      startHls();
    }
  } catch (e) {}
  setTimeout(pollAv, 2000);
}
/* DOM 镜像模式（Phase 2 原型）：快照建镜像、差分按 id 应用于
   沙箱 iframe；输入在镜像内监听后按页面坐标转发（dom:1）。
   快照失败/超时自动回视频模式。 */
const mir = document.getElementById('mir');
const dpill = document.getElementById('dpill');
/* DOM 状态提示走舞台内浮丸（头部已是原生工具栏，不再有条带） */
let lastDomStat = '';
function setPill(t) {
  lastDomStat = t;
  dpill.textContent = t;
  dpill.style.display = (domOn && t && t !== 'DOM mirroring') ? 'block' : 'none';
}
let domOn = false, domSeq = 0, domResyncBusy = false, domTimer = 0;
/* 镜像焦点记忆（rig 节点 id + 输入选区）：快照换 documentElement、
   增量删节点都会把镜像焦点打回 body，焦点一丢 Ctrl+V 就无处
   着落（外部粘贴进 DOM 在动态页面上失效的主因——静态页不重建
   节点所以测不出来）。快照/增量应用后按 id 把焦点与选区恢复
   到替身节点上；只动观看端本地焦点，不给远端发任何东西 */
let mirFocusId = 0, mirFocusSel = null, mirFocusSig = null;
/* 镜像文档渲染模式（compatMode）：镜像 iframe 出身 about:blank
   恒为怪异模式（BackCompat），与远端标准模式页面的流式布局
   系统性漂移——实测 h1 边距差使输入框点击落点偏上 14px、远端
   焦点落空、粘贴/输入随之失效（小目标必偏、大文本框偶尔蒙对，
   是 DOM 模式「时灵时不灵」的根源）。快照带远端 mode，不一致
   时用 srcdoc 重载镜像文档对齐模式，快照暂存待 load 后应用 */
let mirMode = 'BackCompat', pendingSnap = null;
/* 镜像布局（2026-11-20 修 LIU 实测点击左偏 100+px）：iframe
   的布局视口必须等于远端视口（快照 w/h），再整体等比缩放进
   舞台。旧做法镜像填满舞台按舞台 CSS 宽度自行排版——观看端
   DPR≠1 时远端视口=舞台×DPR，两边布局（断点、序列化冻结的
   固定像素尺寸）系统性错位，镜像内 clientX 直送远端必然偏。
   布局视口对齐后镜像与远端逐像素同构：clientX 即远端布局
   坐标、输入转发口径不变；只有视觉换算（幽灵指针/右键菜单）
   需要按缩放比补正。 */
let mirVW = 0, mirVH = 0;
function layoutMirror() {
  const st = document.getElementById('stage');
  if (!mirVW || !mirVH || !st || !st.clientWidth || !st.clientHeight) {
    mir.style.inset = '0'; mir.style.width = '100%'; mir.style.height = '100%';
    mir.style.left = ''; mir.style.top = ''; mir.style.transform = '';
    return;
  }
  const s = Math.min(st.clientWidth / mirVW, st.clientHeight / mirVH);
  mir.style.inset = '';
  mir.style.width = mirVW + 'px'; mir.style.height = mirVH + 'px';
  mir.style.left = Math.round((st.clientWidth - mirVW * s) / 2) + 'px';
  mir.style.top = Math.round((st.clientHeight - mirVH * s) / 2) + 'px';
  mir.style.transform = 'scale(' + s + ')';
}
addEventListener('resize', layoutMirror);
mir.addEventListener('load', () => {
  if (pendingSnap) { const m = pendingSnap; pendingSnap = null; applySnapshot(m); }
});
function restoreMirFocus() {
  if (!mirFocusId && !mirFocusSig) return;
  if (document.activeElement !== mir && document.activeElement !== document.body) return;
  const doc = activeMirrorDocument();
  if (!doc) return;
  const ae = doc.activeElement;
  if (ae && ae !== doc.body && ae !== doc.documentElement) return;
  let el = mirFocusId ? mirMap.get(mirFocusId) : null;
  if ((!el || !el.isConnected) && mirFocusSig && mirFocusSig.idAttr) {
    /* 节点被整体替换（框架重渲染）时 rig id 是新分配的，按
       HTML id 属性找替身——表单控件的 id 跨重渲染通常稳定 */
    const cand = doc.getElementById(mirFocusSig.idAttr);
    if (cand && cand.tagName === mirFocusSig.tag) el = cand;
  }
  if (!el || !el.isConnected || typeof el.focus !== 'function') return;
  try {
    el.focus({ preventScroll: true });
    if (mirFocusSel && (el.tagName === 'INPUT' || el.tagName === 'TEXTAREA') && typeof el.setSelectionRange === 'function') {
      const len = (el.value || '').length;
      el.setSelectionRange(Math.min(mirFocusSel[0], len), Math.min(mirFocusSel[1], len));
    }
  } catch (e) {}
}
let mirMap = new Map();
let modeAuto = true, modeLock = false, modePage = '';
let summaryRequest = 0, summaryPending = null;
let autoWant = null, autoSince = 0, lastAutoSwitch = 0, autoFlips = 0;
const AUTO_HOLD_MS = 3000, AUTO_MIN_INTERVAL_MS = 3000, AUTO_MAX_FLIPS = 2;

function resetAutoMode() {
  modeLock = false;
  summaryPending = null;
  autoWant = null; autoFlips = 0; lastAutoSwitch = 0;
}
function requestModeSummary() {
  if (!modeAuto || modeLock || !modePage || ws.readyState !== 1) return;
  const now = performance.now();
  if (summaryPending && now - summaryPending.at < 3000) return;
  const request = ++summaryRequest;
  summaryPending = {request, at: now};
  send({type:'mode-summary', request});
}
function applyModeSummary(m) {
  if (!summaryPending || m.request !== summaryPending.request) return;
  summaryPending = null;
  if (!modeAuto || modeLock) return;
  const st = { video:m.videoRatio, region:m.regionRatio, canvas:m.canvasRatio, interactive:m.interactive };
  const wantDom = m.supported && !((st.video > 0.10 && st.interactive < 3) || st.region > 0.30 || st.canvas > 0.20);
  window.__rigAuto = {...st, wantDom, url:(hdrState.tabs[hdrState.active]?.url || '')};
  const now = performance.now();
  const first = autoWant === null;
  if (wantDom !== autoWant) { autoWant = wantDom; autoSince = now; }
  if (!first && (now - autoSince < AUTO_HOLD_MS || now - lastAutoSwitch < AUTO_MIN_INTERVAL_MS)) return;
  if (autoFlips >= AUTO_MAX_FLIPS || wantDom === domOn) return;
  autoFlips++;
  lastAutoSwitch = now;
  if (wantDom) {
    domStart(true); flashHint('Auto mode: switched to DOM');
  } else {
    domStop(true); flashHint('Auto mode: switched to video');
  }
}
setInterval(requestModeSummary, 1000);

function domStart(auto) {
  if (!auto) modeLock = true;
  if (domOn) return;
  resetInput();
  domOn = true; domSeq = 0; mirMap = new Map();
  if (hlsRequested) stopHls();
  showDisplay();
  send({type:'dom-start'});
  rtcStart();
  setPill('Waiting for snapshot…');
  clearTimeout(domTimer);
  domTimer = setTimeout(() => { if (domOn && !mirMap.size) domFallback('Snapshot timed out'); }, 8000);
}
function domStop(silent) {
  if (!domOn) return;
  if (!silent) modeLock = true;
  resetInput();
  domOn = false;
  send({type:'dom-stop'});
  mirFocusId = 0; mirFocusSel = null; mirFocusSig = null;
  chunkBuf.clear(); chunkBytes = 0;
  hideGhost();
  clearTimeout(domTimer);
  showDisplay();
  if (!silent) rtcStart();
}
/* 幽灵指针叠加：边缘下发的是页面坐标，减去镜像当前滚动、
   加上 iframe 在视口中的偏移即得叠加位置 */
const ghostEl = document.getElementById('ghost');
const glabel = document.getElementById('glabel');
let ghostPos = null;
function showGhost(m) {
  ghostPos = { x: m.x, y: m.y, trail: m.trail || [] };
  glabel.textContent = m.label || '';
  ghostEl.className = m.pulse ? 'pulse' : '';
  placeGhost();
}
function hideGhost() {
  ghostPos = null;
  ghostEl.style.display = 'none';
  for (const d of trailPool) d.style.display = 'none';
}
/* 页面坐标 → 客户端坐标（按当前显示面映射：镜像按其布局+
   滚动，画布/视频按元素框与帧尺寸比例） */
function ghostClient(x, y) {
  if (domOn) {
    const r = mir.getBoundingClientRect();
    let sx = 0, sy = 0;
    try { sx = mir.contentWindow.scrollX || 0; sy = mir.contentWindow.scrollY || 0; } catch (e) {}
    /* r 是缩放后的视觉矩形：布局坐标差要乘缩放比才是视觉偏移 */
    const k = mirVW ? r.width / mirVW : 1;
    return { x: r.left + (x - sx) * k, y: r.top + (y - sy) * k };
  }
  const r = frameRect();
  return { x: r.left + x * r.width / Math.max(1, cssViewportW),
           y: r.top + y * r.height / Math.max(1, cssViewportH) };
}
const trailPool = [];
function placeGhost() {
  if (!ghostPos) { ghostEl.style.display = 'none'; return; }
  const p = ghostClient(ghostPos.x, ghostPos.y);
  ghostEl.style.left = p.x + 'px';
  ghostEl.style.top = p.y + 'px';
  ghostEl.style.display = 'block';
  /* 轨迹点（旧→新，不透明度由边缘按龄给出）：池化小圆点 */
  const tr = ghostPos.trail || [];
  while (trailPool.length < tr.length) {
    const d = document.createElement('div');
    d.className = 'gtrail';
    document.body.appendChild(d);
    trailPool.push(d);
  }
  trailPool.forEach((d, i) => {
    if (i >= tr.length) { d.style.display = 'none'; return; }
    const q = ghostClient(tr[i][0], tr[i][1]);
    d.style.left = q.x + 'px';
    d.style.top = q.y + 'px';
    d.style.opacity = String(tr[i][2]);
    d.style.display = 'block';
  });
}
function domFallback(why) {
  domStop(true);
  setPill('DOM mirror unavailable: ' + why);
  rtcStart();
}
/* 区域子流占位块：跨源 iframe（序列化读不到内容）与 video 元素
   在镜像里只是盒子，边缘按 rect 截图串流（dom-region）贴图进来。
   regionCache 跨快照重建保留最后一帧，建节点时直接贴上。
   D04：缓存按页面代次隔离——快照 URL 变化（导航/换文档，页内
   序列化器 id 从 1 重排）即清空，不把旧页的图贴给新文档；
   最多 128 项，防驻留图像堆积。条目自带代次，跨代一律不用。 */
const regionCache = new Map();
let regionEpoch = '';
const REGION_CACHE_MAX = 128;
function regionEpochCheck(url) {
  const u = url || '';
  if (u !== regionEpoch) { regionEpoch = u; regionCache.clear(); }
}
function regionPut(id, dataUrl) {
  regionCache.set(id, { d: dataUrl, e: regionEpoch });
  if (regionCache.size > REGION_CACHE_MAX) {
    // 删最旧（Map 按插入序）
    regionCache.delete(regionCache.keys().next().value);
  }
}
function regionGet(id) {
  const c = regionCache.get(id);
  return (c && c.e === regionEpoch) ? c.d : undefined;
}
function regionBox(n, doc) {
  const el = doc.createElement('div');
  for (const k in (n.a || {})) {
    if (k === 'src' || k === 'srcdoc') continue;
    try { el.setAttribute(k, n.a[k]); } catch (e) {}
  }
  const w = n.rect ? n.rect[0] : 300, h = n.rect ? n.rect[1] : 150;
  el.style.position = 'relative';
  el.style.overflow = 'hidden';
  /* 保持原元素的 display 性质（iframe/video 多为行内级）：占位
     块若变成块级会把行内流挤换行，镜像布局与远端分叉。但原
     元素是「行内替换元素」——inline 也认宽高；普通 div 的
     inline 不认宽高会塌成 0×0，故 inline 一律映射 inline-block */
  el.style.display = (!n.disp || n.disp === 'inline') ? 'inline-block' : n.disp;
  if (!el.style.width) el.style.width = w + 'px';
  if (!el.style.height) el.style.height = h + 'px';
  if (n.g === 'VIDEO') el.style.background = '#000';
  el.setAttribute('data-rig-region', n.i);
  const img = doc.createElement('img');
  img.setAttribute('style', 'position:absolute;inset:0;width:100%;height:100%;display:none');
  img.setAttribute('data-rig-region-img', '1');
  const poster = n.a && n.a.poster;
  if (poster) { img.src = poster; img.style.display = 'block'; }
  const cached = regionGet(n.i);
  if (cached) { img.src = cached; img.style.display = 'block'; }
  el.appendChild(img);
  return el;
}
function buildNode(n, doc) {
  if (n.t !== undefined) { const tn = doc.createTextNode(n.t); tn.__rigId = n.i; mirMap.set(n.i, tn); return tn; }
  let el;
  if ((n.g === 'IFRAME' && n.x) || n.g === 'VIDEO' || (n.g === 'CANVAS' && n.big)) {
    el = regionBox(n, doc);
    el.__rigId = n.i;
    mirMap.set(n.i, el);
    return el;
  }
  if (n.g === 'CANVAS') {
    el = doc.createElement('div');
    const w = n.rect ? n.rect[0] : 300, h = n.rect ? n.rect[1] : 150;
    el.setAttribute('style', 'width:' + w + 'px;height:' + h + 'px;background:#20242c;color:#9ab;display:flex;align-items:center;justify-content:center;font:12px sans-serif');
    el.textContent = 'CANVAS ' + w + '×' + h;
    el.setAttribute('data-rig-canvas', '1');
  } else {
    // SVG 子树按命名空间建元素且标签名不 lowercase
    // （foreignObject 等大小写敏感），否则图标整体不渲染
    el = n.ns ? doc.createElementNS('http://www.w3.org/2000/svg', n.g)
              : doc.createElement((n.g || 'div').toLowerCase());
    for (const k in (n.a || {})) {
      // iframe 不带 src/srcdoc：镜像不许它真的导航去加载源页面，
      // 内容由 f 序列化树注入（见下）
      if (n.g === 'IFRAME' && (k === 'src' || k === 'srcdoc')) continue;
      try { el.setAttribute(k, n.a[k]); } catch (e) {}
    }
    if (n.v !== undefined && (n.g === 'INPUT' || n.g === 'TEXTAREA' || n.g === 'SELECT')) { try { el.value = n.v; } catch (e) {} }
    if (n.k && n.g === 'INPUT') { try { el.checked = true; } catch (e) {} }
    if (n.k && n.g === 'OPTION') { try { el.selected = true; } catch (e) {} }
  }
  el.__rigId = n.i;
  mirMap.set(n.i, el);
  for (const c of (n.c || [])) el.appendChild(buildNode(c, doc));
  if (n.g === 'IFRAME' && n.f) {
    // 尚未挂载的 iframe 没有内部文档。以本地空 srcdoc 套用原文档模式，避免加载受控站点。
    el.addEventListener('load', () => {
      const inner = el.contentDocument;
      if (!domOn || !el.isConnected || inner?.URL !== 'about:srcdoc') return;
      inner.replaceChild(buildNode(n.f, inner), inner.documentElement);
      attachMirrorInput(inner);
      restoreMirFocus();
    });
    el.srcdoc = (n.fm === 'CSS1Compat' ? '<!doctype html>' : '') + '<html><head></head><body></body></html>';
  }
  return el;
}
function applySnapshot(m) {
  if (m.mode && m.mode !== mirMode) {
    mirMode = m.mode;
    pendingSnap = m;
    mir.srcdoc = (m.mode === 'CSS1Compat' ? '<!doctype html>' : '') + '<html><head></head><body></body></html>';
    return;
  }
  const doc = mir.contentDocument;
  if (!doc || !m.tree) return;
  mirMap = new Map();
  const root = buildNode(m.tree, doc);
  // <base> 必须在整棵树接入文档之前就位：树里的 <link>/<img> 在
  // 接入瞬间按当时的文档基准解析 URL，base 晚到一步，根相对
  // 路径就落到观看页的源上（实测 YouTube 播放器 CSS 被请求到
  // 边缘地址吃 401）。序列化端已把资源 URL 绝对化，这里是双
  // 保险（CSS 文本内未覆盖到的引用靠它兜底）。
  let head = null;
  for (const ch of root.childNodes) { if (ch.tagName === 'HEAD') { head = ch; break; } }
  if (!head) { head = doc.createElement('head'); root.insertBefore(head, root.firstChild); }
  head.innerHTML = '';
  const base = doc.createElement('base'); base.href = m.url || ''; head.appendChild(base);
  /* 子资源（图片/CSS 背景）的 Referer 是观看页源，图片 CDN
     普遍按外来 Referer 防盗链（实测 Bilibili hdslb 缩略图全灭、
     远端页带本站 Referer 正常）；镜像无法也不应伪造来源，
     统一 no-referrer（同图实测放行） */
  const rmeta = doc.createElement('meta'); rmeta.name = 'referrer'; rmeta.content = 'no-referrer'; head.appendChild(rmeta);
  for (const href of (m.links || [])) {
    const l = doc.createElement('link'); l.rel = 'stylesheet'; l.href = href; head.appendChild(l);
  }
  const st = doc.createElement('style'); st.textContent = m.css || ''; head.appendChild(st);
  doc.replaceChild(root, doc.documentElement);
  mir.width = m.w || 1280; mir.height = m.h || 800;
  mirVW = m.w || cssViewportW; mirVH = m.h || cssViewportH;
  layoutMirror();
  syncAddr(m.url || '');
  attachMirrorInput(doc);
  restoreMirFocus();
  if (mir.contentWindow) mir.contentWindow.scrollTo(m.sx || 0, m.sy || 0);
  placeGhost();
  domSeq = m.seq; domResyncBusy = false;
  clearTimeout(domTimer);
  setPill('DOM mirroring');
}
function applyDelta(m) {
  if (m.seq !== domSeq + 1) {
    if (!domResyncBusy) { domResyncBusy = true; send({type:'dom-resync'}); }
    return;
  }
  domSeq = m.seq;
  const doc = mir.contentDocument;
  if (!doc) return;
  for (const op of (m.ops || [])) {
    const node = mirMap.get(op.i);
    if (op.o === 'rm') {
      if (node) {
        // 整棵子树的映射一并清（R-08：只删根会把后代引用留在
        // mirMap 里，SPA 长开持续滞留）
        const stack = [node];
        while (stack.length) {
          const n = stack.pop();
          if (n.__rigId !== undefined) mirMap.delete(n.__rigId);
          if (n.childNodes) for (const c of n.childNodes) stack.push(c);
          if (n.tagName === 'IFRAME' && n.contentDocument?.documentElement) {
            stack.push(n.contentDocument.documentElement);
          }
        }
        if (node.parentNode) node.parentNode.removeChild(node);
      }
      mirMap.delete(op.i);
    } else if (op.o === 'add') {
      const p = mirMap.get(op.p);
      if (p) {
        const nn = buildNode(op.n, p.ownerDocument);
        const before = op.b ? mirMap.get(op.b) : null;
        p.insertBefore(nn, before || null);
      }
    } else if (op.o === 'at' && node && node.setAttribute) {
      if (node.tagName === 'IFRAME' && ['src', 'srcdoc'].includes(op.n)) continue;
      if (op.v === null || op.v === undefined) node.removeAttribute(op.n);
      else node.setAttribute(op.n, op.v);
    } else if (op.o === 'tx' && node) {
      node.data = op.v;
    } else if (op.o === 'val' && node) {
      if (node.ownerDocument.activeElement !== node) {
        try { node.value = op.v; } catch (e) {}
        if ('checked' in node) node.checked = !!op.k;
      }
    } else if (op.o === 'scr') {
      if (op.i === 0) { if (mir.contentWindow) mir.contentWindow.scrollTo(op.x, op.y); placeGhost(); }
      else if (node) { node.scrollLeft = op.x; node.scrollTop = op.y; }
    }
  }
  restoreMirFocus();
}
function attachMirrorInput(doc) {
  /* 同一镜像文档只挂一次：applySnapshot 只换 documentElement、
     文档对象本身跨快照存续，每份快照重挂一遍会让每个手势被转
     发多次（视频点一下变播放+暂停、菜单开两次、输入双倍工作
     ——DOM 发卡与点击失效的一个真实来源） */
  if (doc.__rigAttached) return;
  doc.__rigAttached = true;
  const cap = { capture: true };
  doc.addEventListener('pointerdown', resumeVid, cap);
  doc.addEventListener('keydown', resumeVid, cap);
  const pos = (e) => mirrorRootPoint(doc, e.clientX, e.clientY);
  const editable = (t) => t && (t.tagName === 'INPUT' || t.tagName === 'TEXTAREA' || t.tagName === 'SELECT' || t.isContentEditable);
  /* 焦点记忆：focusin 记下可编辑目标的 rig id（mirMap 反查，
     焦点事件低频、线性扫可接受），selectionchange 跟踪输入
     选区；恢复动作在 restoreMirFocus（快照/增量后调用） */
  doc.addEventListener('focusin', e => {
    const target = e.target;
    mirFocusId = target.__rigId ?? 0;
    mirFocusSel = null;
    mirFocusSig = target.id ? {tag:target.tagName, idAttr:target.id} : null;
  }, cap);
  doc.addEventListener('selectionchange', () => {
    const a = doc.activeElement;
    if (a && (a.tagName === 'INPUT' || a.tagName === 'TEXTAREA') && typeof a.selectionStart === 'number') {
      mirFocusSel = [a.selectionStart, a.selectionEnd];
    }
  });
  doc.addEventListener('mousedown', e => {
    if (!domOn) return;
    if (e.button !== 0 && e.button !== 2) e.preventDefault();
    closeCtx();
    mouseButton(e, 'down', pos(e), true);
  }, cap);
  doc.addEventListener('mouseup', e => { if (domOn) mouseButton(e, 'up', pos(e), true); }, cap);
  doc.addEventListener('mousemove', e => { if (domOn) movePointer(e, pos(e), true); }, cap);
  doc.addEventListener('mouseleave', e => {
    if (domOn && !heldButtons && e.target === doc.documentElement) movePointer(e, {x:-1, y:-1}, true);
  }, cap);
  doc.addEventListener('wheel', e => {
    if (!domOn) return;
    e.preventDefault();
    send({type:'wheel', dom:true, ...wheelPixels(e), modifiers:mods(e), ...pos(e)});
  }, {capture:true, passive:false});
  doc.addEventListener('dragstart', e => { if (domOn) e.preventDefault(); }, cap);
  doc.addEventListener('keydown', e => {
    if (viewerShortcut(e, doc)) return;
    keyDown(e, true, editable(e.target) || e.key === 'Tab' || ((e.ctrlKey || e.metaKey) && e.key.toLowerCase() === 'a'));
  }, cap);
  doc.addEventListener('paste', (e) => {
    const cd = e.clipboardData || window.clipboardData;
    if (!cd) return;
    const img = [...(cd.files || [])].find(f => f.type.startsWith('image/'));
    if (img) {
      e.preventDefault();
      const rd = new FileReader();
      rd.onload = () => send({type:'paste-image', data: String(rd.result).split(',')[1]});
      rd.readAsDataURL(img);
      return;
    }
    const t = cd.getData('text/plain');
    if (t) send({type:'text', dom:true, text: t});
    /* 文本不壓默認動作：鏡像需要原生粘貼作本地回顯（與打字同
       口徑）。遠端靠上面的 send 落字；鏡像聚焦元素的 val 回環
       按設計跳過（防打字回退），若這裡也壓掉，粘貼在鏡像裡
       永遠不顯示——LIU 實測「DOM 模式外→內無法粘貼」的真因：
       遠端其實已落字，只是鏡像看不見。 */
  }, cap);
  /* DOM 模式复制的是镜像自己的选区（本地文档、同步可读）：
     事件内以镜像选区兑现本地剪贴板，永远准确，不需要回远端拉 */
  doc.addEventListener('copy', (e) => {
    const t = mirrorSelText(doc);
    if (t) { e.preventDefault(); e.clipboardData.setData('text/plain', t); }
  }, cap);
  doc.addEventListener('cut', (e) => {
    const t = mirrorSelText(doc);
    if (t) { e.preventDefault(); e.clipboardData.setData('text/plain', t); }
  }, cap);
  doc.addEventListener('keyup', e => keyUp(e, true), cap);
  doc.addEventListener('compositionend', (e) => { if (e.data) send({type:'text', dom:true, text: e.data}); }, cap);
  for (const type of ['click', 'auxclick']) {
    doc.addEventListener(type, e => {
      if (e.target?.closest?.('a') || e.button >= 3) e.preventDefault();
    }, cap);
  }
  doc.addEventListener('contextmenu', e => {
    if (!domOn) return;
    e.preventDefault();
    mirrorContext(doc, e.clientX, e.clientY, e.target);
  }, cap);
}
document.getElementById('domBtn').onclick = () => { if (domOn) domStop(); else domStart(); };

/* ── 原生工具栏（第五期）──────────────────────────────────
   边缘只下发 chrome-state（标签/导航可用性/加载态/寻找会话/
   分辨率/通道勾选），本节渲染并以 chrome 命令回送操作。
   地址栏/查找框是真输入框：编辑、右键菜单、粘贴全走浏览器
   原生行为（合成条带时代靠边缘编辑态模拟的那套整删）。 */
const SVGNS = 'http://www.w3.org/2000/svg';
function svgIcon(paths, fill) {
  const s = document.createElementNS(SVGNS, 'svg');
  s.setAttribute('viewBox', '0 0 24 24');
  if (!fill) {
    s.setAttribute('fill', 'none');
    s.setAttribute('stroke', 'currentColor');
    s.setAttribute('stroke-width', '2');
    s.setAttribute('stroke-linecap', 'round');
    s.setAttribute('stroke-linejoin', 'round');
  } else {
    s.setAttribute('fill', 'currentColor');
  }
  for (const d of paths) {
    if (d.startsWith('CIRCLE:')) {
      const c = document.createElementNS(SVGNS, 'circle');
      const [, cx, cy, r] = d.split(':');
      c.setAttribute('cx', cx); c.setAttribute('cy', cy); c.setAttribute('r', r);
      s.appendChild(c);
    } else {
      const p = document.createElementNS(SVGNS, 'path');
      p.setAttribute('d', d);
      s.appendChild(p);
    }
  }
  return s;
}
const ICO = {
  back: ['M19 12H5', 'M11 18l-6-6 6-6'],
  fwd: ['M5 12h14', 'M13 6l6 6-6 6'],
  reload: ['M20 12a8 8 0 1 1-2.3-5.6', 'M20 3v5h-5'],
  stop: ['M6 6l12 12', 'M18 6L6 18'],
  plus: ['M12 5v14', 'M5 12h14'],
  close: ['M6 6l12 12', 'M18 6L6 18'],
  lock: ['M7 11V8a5 5 0 0 1 10 0v3', 'M5 11h14v10H5z'],
  globe: ['CIRCLE:12:12:9', 'M3 12h18', 'M12 3a15 15 0 0 1 0 18', 'M12 3a15 15 0 0 0 0 18'],
  dots: ['CIRCLE:12:5:1.7', 'CIRCLE:12:12:1.7', 'CIRCLE:12:19:1.7'],
  chevUp: ['M6 15l6-6 6 6'],
  chevDn: ['M6 9l6 6 6-6'],
};
const backBtn = document.getElementById('backBtn');
const fwdBtn = document.getElementById('fwdBtn');
const reloadBtn = document.getElementById('reloadBtn');
const addrEl = document.getElementById('addr');
const omniIcon = document.getElementById('omniIcon');
const tabsEl = document.getElementById('tabs');
const loadbar = document.getElementById('loadbar');
const menuEl = document.getElementById('menu');
const findbar = document.getElementById('findbar');
const findInput = document.getElementById('findInput');
const findInfo = document.getElementById('findInfo');
backBtn.appendChild(svgIcon(ICO.back));
fwdBtn.appendChild(svgIcon(ICO.fwd));
reloadBtn.appendChild(svgIcon(ICO.reload));
document.getElementById('newTabBtn').appendChild(svgIcon(ICO.plus));
document.getElementById('menuBtn').appendChild(svgIcon(ICO.dots, true));
document.getElementById('findPrev').appendChild(svgIcon(ICO.chevUp));
document.getElementById('findNext').appendChild(svgIcon(ICO.chevDn));
document.getElementById('findClose').appendChild(svgIcon(ICO.close));
omniIcon.appendChild(svgIcon(ICO.globe));

function chromeCmd(action, extra) {
  if (['back', 'forward', 'reload', 'navigate', 'new-tab', 'switch-tab', 'close-tab', 'open-internal'].includes(action)) {
    resetInput();
    closeCtx();
  }
  send({type:'chrome', action, ...(extra || {})});
}
let hdrState = null;
window.__rigChrome = null;

function syncAddr(url) {
  if (document.activeElement !== addrEl) addrEl.value = url || '';
  const https = (url || '').startsWith('https://');
  omniIcon.replaceChildren(svgIcon(https ? ICO.lock : ICO.globe));
}
function renderTabs(st) {
  tabsEl.replaceChildren();
  (st.tabs || []).forEach((t, i) => {
    const d = document.createElement('div');
    d.className = 'tab' + (i === st.active ? ' active' : '');
    d.title = t.url || '';
    const fav = document.createElement('span');
    fav.className = 'fav';
    if (st.loading && i === st.active) {
      const sp = document.createElement('span');
      sp.className = 'spin';
      fav.appendChild(sp);
    } else {
      fav.appendChild(svgIcon(ICO.globe));
    }
    const ttl = document.createElement('span');
    ttl.className = 'ttl';
    ttl.textContent = t.title || t.url || 'New tab';
    const cls = document.createElement('button');
    cls.className = 'cls';
    cls.title = 'Close tab';
    cls.appendChild(svgIcon(ICO.close));
    cls.addEventListener('mousedown', (e) => e.stopPropagation());
    cls.addEventListener('click', (e) => { e.stopPropagation(); chromeCmd('close-tab', { index: i }); });
    d.appendChild(fav); d.appendChild(ttl); d.appendChild(cls);
    d.addEventListener('mousedown', (e) => {
      if (e.button === 1) { e.preventDefault(); chromeCmd('close-tab', { index: i }); }
    });
    d.addEventListener('click', () => { if (i !== st.active) chromeCmd('switch-tab', { index: i }); });
    tabsEl.appendChild(d);
  });
}
function applyChromeState(m) {
  const previous = hdrState;
  hdrState = m;
  window.__rigChrome = m;
  renderTabs(m);
  backBtn.disabled = !m.canBack;
  fwdBtn.disabled = !m.canFwd;
  reloadBtn.replaceChildren(svgIcon(m.loading ? ICO.stop : ICO.reload));
  reloadBtn.title = m.loading ? 'Stop loading' : 'Reload';
  loadbar.classList.toggle('go', !!m.loading);
  const at = (m.tabs || [])[m.active || 0];
  const page = m.active + ':' + (at?.url || '');
  if (!modePage) {
    modePage = page;
    requestModeSummary();
  } else if (page !== modePage || (m.loading && !previous?.loading)) {
    modePage = page;
    mirFocusId = 0; mirFocusSel = null; mirFocusSig = null;
    resetAutoMode();
    requestModeSummary();
  }
  if (previous && (previous.res !== m.res || previous.active !== m.active)) {
    resizeSent = null;
    reportSize();
  }
  syncAddr(at ? at.url : '');
  if (m.find) {
    findbar.classList.add('open');
    findInfo.textContent = m.find.info || '';
    if (document.activeElement !== findInput && findInput.value !== (m.find.text || '')) {
      findInput.value = m.find.text || '';
    }
  } else {
    findbar.classList.remove('open');
  }
}
backBtn.onclick = () => chromeCmd('back');
fwdBtn.onclick = () => chromeCmd('forward');
reloadBtn.onclick = () => chromeCmd('reload');
document.getElementById('newTabBtn').onclick = () => chromeCmd('new-tab');
addrEl.addEventListener('focus', () => addrEl.select());
addrEl.addEventListener('keydown', (e) => {
  e.stopPropagation();
  if (e.isComposing || e.keyCode === 229) return;
  if (e.key === 'Enter') {
    chromeCmd('navigate', { url: addrEl.value });
    addrEl.blur();
    focusPage();
  } else if (e.key === 'Escape') {
    const at = hdrState && (hdrState.tabs || [])[hdrState.active || 0];
    addrEl.value = at ? at.url : '';
    addrEl.blur();
    focusPage();
  }
});
addrEl.addEventListener('mousedown', (e) => e.stopPropagation());
/* ── 主菜单（项与旧合成菜单一一对应，勾选态取 chrome-state）── */
let menuOpen = false;
function closeMenu() { menuOpen = false; menuEl.classList.remove('open'); }
function buildMenu() {
  const st = hdrState || { res: 'auto', gateAi: true, gateHuman: true };
  menuEl.replaceChildren();
  const item = (label, fn, opts) => {
    const b = document.createElement('button');
    const ck = document.createElement('span');
    ck.className = 'ck';
    ck.textContent = opts && opts.checked ? '✓' : '';
    const tx = document.createElement('span');
    tx.textContent = label;
    b.appendChild(ck); b.appendChild(tx);
    if (opts && opts.sc) {
      const sc = document.createElement('span');
      sc.className = 'sc';
      sc.textContent = opts.sc;
      b.appendChild(sc);
    }
    b.onclick = () => { closeMenu(); fn(); };
    menuEl.appendChild(b);
  };
  const sep = () => menuEl.appendChild(document.createElement('hr'));
  item('Find in page', openFind, { sc: 'Ctrl+F' });
  item('New tab', () => chromeCmd('new-tab'));
  item('Downloads', () => chromeCmd('open-internal', { url: 'chrome://downloads' }));
  item('Extensions', () => chromeCmd('open-internal', { url: 'chrome://extensions' }));
  item('History', () => chromeCmd('open-internal', { url: 'chrome://history' }));
  item('Settings', () => chromeCmd('open-internal', { url: 'chrome://settings' }));
  item('Print…', () => chromeCmd('print'));
  sep();
  item('Resolution: follow window', () => chromeCmd('res-auto'), { checked: st.res === 'auto' });
  item('Resolution: 1280 × 800', () => chromeCmd('res-fixed', { w: 1280, h: 800 }), { checked: st.res === '1280x800' });
  item('Resolution: 1920 × 1080', () => chromeCmd('res-fixed', { w: 1920, h: 1080 }), { checked: st.res === '1920x1080' });
  sep();
  item('AI channel', () => chromeCmd('toggle-ai'), { checked: !!st.gateAi });
  item('Human channel', () => chromeCmd('toggle-human'), { checked: !!st.gateHuman });
}
document.getElementById('menuBtn').onclick = (e) => {
  e.stopPropagation();
  if (menuOpen) { closeMenu(); return; }
  buildMenu();
  menuOpen = true;
  menuEl.classList.add('open');
};
document.addEventListener('pointerdown', (e) => {
  if (menuOpen && !menuEl.contains(e.target)) closeMenu();
}, true);
function focusPage() {
  if (!domOn) { ime.focus({preventScroll:true}); return; }
  const doc = activeMirrorDocument();
  doc?.defaultView?.focus();
  doc?.activeElement?.focus({preventScroll:true});
}

/* ── 查找条 ── */
function openFind() {
  findbar.classList.add('open');
  chromeCmd('find-open');
  findInput.focus();
  findInput.select();
}
function closeFind() {
  findbar.classList.remove('open');
  chromeCmd('find-close');
  focusPage();
}
let findTextTimer = 0;
findInput.addEventListener('input', () => {
  clearTimeout(findTextTimer);
  findTextTimer = setTimeout(() => chromeCmd('find-text', { text: findInput.value }), 150);
});
findInput.addEventListener('keydown', (e) => {
  e.stopPropagation();
  if (e.isComposing || e.keyCode === 229) return;
  if (e.key === 'Enter') chromeCmd(e.shiftKey ? 'find-prev' : 'find-next');
  else if (e.key === 'Escape') closeFind();
});
findInput.addEventListener('mousedown', (e) => e.stopPropagation());
document.getElementById('findNext').onclick = () => chromeCmd('find-next');
document.getElementById('findPrev').onclick = () => chromeCmd('find-prev');
document.getElementById('findClose').onclick = closeFind;
/* 浏览器快捷键归观看端工具栏。镜像 iframe 也需要自己的监听器。 */
function viewerShortcut(e, mirrorDoc) {
  if (contextMenuKey(e)) return true;
  if (e.isComposing || e.keyCode === 229 || e.getModifierState('AltGraph')) return false;
  const primary = e.ctrlKey || e.metaKey;
  const key = e.key.toLowerCase();
  let action;
  if (!primary && !e.altKey && (key === 'contextmenu' || (key === 'f10' && e.shiftKey)) &&
      (mirrorDoc || e.target === ime || ctxEl)) {
    action = () => {
      if (mirrorDoc) {
        const active = mirrorDoc.activeElement;
        const rect = active.getBoundingClientRect();
        mirrorContext(mirrorDoc, rect.left, rect.bottom, active, true);
      } else {
        const rect = frameRect();
        openCtx(rect.left + rect.width / 2, rect.top + rect.height / 2, null, ime, true);
      }
    };
  } else if (primary && !e.altKey && !e.shiftKey) {
    if (key === 'f') action = openFind;
    else if (key === 'l') action = () => { addrEl.focus(); addrEl.select(); };
    else if (key === 'r') action = () => chromeCmd('reload');
    else if (key === 't') action = () => chromeCmd('new-tab');
    else if (key === 'w') action = () => {
      if (hdrState) chromeCmd('close-tab', {index:hdrState.active});
    };
    else if (key === 'pageup' || key === 'pagedown') action = () => {
      if (!hdrState?.tabs.length) return;
      const count = hdrState.tabs.length;
      const index = (hdrState.active + (key === 'pageup' ? -1 : 1) + count) % count;
      chromeCmd('switch-tab', {index});
    };
  } else if (e.altKey && !primary && !e.shiftKey) {
    if (key === 'arrowleft') action = () => chromeCmd('back');
    if (key === 'arrowright') action = () => chromeCmd('forward');
  } else if (!primary && !e.altKey && !e.shiftKey && key === 'f5') {
    action = () => chromeCmd('reload');
  }
  if (!action) return false;
  e.preventDefault();
  e.stopPropagation();
  if (!e.repeat) action();
  return true;
}
document.addEventListener('keydown', e => viewerShortcut(e, null), true);
if (new URLSearchParams(location.search).get('dom') === '1') {
  ws.addEventListener('open', () => domStart());
}
/* 浏览器阻止有声自动播放时，通过观看者手势恢复音频。 */
function resumeVid() {
  if (avWanted && rtcAudio.srcObject && rtcAudio.paused) rtcAudio.play().catch(() => {});
  if (hlsRequested && vid.paused) vid.play().catch(() => {});
}
document.addEventListener('pointerdown', resumeVid, true);
document.addEventListener('keydown', resumeVid, true);
pollAv();
ime.focus();
