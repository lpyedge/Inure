
const cv = document.getElementById('cv'), ctx = cv.getContext('2d');
const vid = document.getElementById('vid');
const ime = document.getElementById('ime');
const promptBox = document.getElementById('prompt');
/* 当前帧的像素尺寸（第五期起为纯页面帧）；坐标换算以此为准 */
let frameW = 1280, frameH = 800;
/* 直连令牌：从本页 URL 的 ?token= 读出，透传给 WS 与文件取回 */
const TOK = new URLSearchParams(location.search).get('token') || '';
const TQ = TOK ? ('?token=' + encodeURIComponent(TOK)) : '';
const fch = (u, o) => fetch(u + (TOK ? (u.includes('?') ? '&' : '?') + 'token=' + encodeURIComponent(TOK) : ''), o);
const ws = new WebSocket((location.protocol === 'https:' ? 'wss://' : 'ws://') + location.host + '/ws' + TQ);
let avOn = false;
/* 进 DOM 前影音是否开着：domStart 会把影音关掉（互斥），退出
   DOM 时据此恢复——不恢复则视频模式恒为无声的 RTC 呈现，
   vid 停在 ontrack 的初始静音上（LIU 2026-11-20 实测无声定因） */
let avWasOn = false;
/* AV 呈现路径（架构口径：WebRTC 优先、HLS 最终兜底）：
   avVia = 'rtc'（视频+Opus 音频走 RTC）| 'hls'（ffmpeg 合流）| null（待定） */
let avVia = null, avWait = 0;
/* U01：DOM 模式是否保留 RTC 声音（默认保留；HLS 无独立声轨，DOM 下静音） */
let domAudio = true;
ws.binaryType = 'arraybuffer';
let frameSeq = 0;
ws.onmessage = (ev) => {
  if (typeof ev.data !== 'string') {
    /* 二进制帧：8 字节头（宽/高，u32 小端）+ JPEG 原始字节。
       序号守卫：解码慢的旧帧不得覆盖已画的新帧 */
    const dv = new DataView(ev.data);
    const w = dv.getUint32(0, true), h = dv.getUint32(4, true);
    frameW = w; frameH = h;
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
  if (m.type === 'rtc-offer' && pc) {
    pc.setRemoteDescription({type:'offer', sdp: m.sdp})
      .then(() => pc.createAnswer())
      .then(a => pc.setLocalDescription(a).then(() => send({type:'rtc-answer', sdp: a.sdp})))
      .catch(() => rtcStop());
  }
  if (m.type === 'rtc-state' && m.state === 'disconnected') {
    /* 服务端视角断开且本地未连通：切回 canvas（WS 帧已由服务端恢复） */
    if (!pc || pc.connectionState !== 'connected') { rtcOn = false; showDisplay(); }
  }
  if (m.type === 'dom-snapshot' || m.type === 'dom-delta' || m.type === 'dom-state' || m.type === 'dom-canvas' || m.type === 'dom-region' || m.type === 'dom-chunk') {
    __domStats.ws++; domFeed(m);
  }
  if (m.type === 'ghost') showGhost(m);
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
/* D03 有界：残缺包 10s 过期回收、最多 16 个在途重组、总量 8MB；
   跨 RTC 会话换代时 clear（见 rtcStop）。超限/过期一律丢弃并
   请求全量快照，不拿半条拼。序号/数量/类型先校验，不信任远端。 */
const CHUNK_MAX_N = 4096, CHUNK_MAX_ENTRIES = 16;
const CHUNK_MAX_BYTES = 8 * 1024 * 1024, CHUNK_TTL_MS = 10000;
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
  if (m.type === 'dom-snapshot') onSnapAuto(m);
  if (m.type === 'dom-snapshot' && domOn) applySnapshot(m);
  if (m.type === 'dom-delta' && domOn) applyDelta(m);
  if (m.type === 'dom-canvas' && domOn) applyCanvas(m);
  if (m.type === 'dom-region' && domOn) applyRegion(m);
  if (m.type === 'dom-state' && m.state === 'error' && domOn) domFallback(m.reason || '边缘错误');
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
document.getElementById('pYes').onclick = () => { promptBox.style.display = 'none'; send({type:'takeover', yes:true}); ime.focus(); };
document.getElementById('pNo').onclick = () => { promptBox.style.display = 'none'; send({type:'takeover', yes:false}); ime.focus(); };
/* 坐标：以当前显示元素（canvas 或 video）的实际帧尺寸换算 */
function toFrame(e) {
  const useVid = avOn || rtcOn;
  const el = useVid ? vid : cv;
  const r = el.getBoundingClientRect();
  const w = useVid ? (vid.videoWidth || frameW) : frameW;
  const h = useVid ? (vid.videoHeight || frameH) : frameH;
  return { x: (e.clientX - r.left) * (w / r.width), y: (e.clientY - r.top) * (h / r.height) };
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
  if (cv.style.display !== 'none') cv.style.transform = t;
  if (vid.style.display !== 'none') vid.style.transform = t;
  clearTimeout(fbScrollTimer);
  fbScrollTimer = setTimeout(fbScrollReset, 180); /* 帧流追上后归位 */
}
ime.addEventListener('mousedown', e => { ime.focus(); if (!domOn && e.button === 0) { fbClick(e.clientX, e.clientY); fbDrag = { x: e.clientX, y: e.clientY }; } send({type:'mouse', action:'down', button: e.button, buttons: e.buttons, count: e.detail, ...toFrame(e)}); });
ime.addEventListener('mouseup',   e => { flushMove(); fbDrag = null; setTimeout(() => { fbRect.style.display = 'none'; }, 120); send({type:'mouse', action:'up',   button: e.button, buttons: e.buttons, count: e.detail, ...toFrame(e)}); });
let lastMove = 0, pendingMove = null, moveTimer = null;
function flushMove() {
  if (moveTimer) { clearTimeout(moveTimer); moveTimer = null; }
  if (pendingMove) { const p = pendingMove; pendingMove = null; lastMove = Date.now(); send({type:'mouse', action:'move', ...p}); }
}
ime.addEventListener('mousemove', e => {
  if (!domOn) fbDragMove(e.clientX, e.clientY);
  const n = Date.now();
  /* F-05：最新值合并——窗口收紧到 ~24ms（原 60ms 拖动明显
     跟不上手）；窗口内只留最新点，到点补发，终点不丢 */
  if (n - lastMove > 24) { lastMove = n; send({type:'mouse', action:'move', buttons: e.buttons, ...toFrame(e)}); return; }
  pendingMove = {buttons: e.buttons, ...toFrame(e)};
  if (!moveTimer) moveTimer = setTimeout(flushMove, 26);
});
ime.addEventListener('wheel', e => { e.preventDefault(); if (!domOn) fbScroll(e.deltaY); send({type:'wheel', dx: e.deltaX, dy: e.deltaY, ...toFrame(e)}); }, {passive:false});
/* 右键菜单归远端页面：本地菜单挡掉，右键按下经 mouse 消息转发，
   页面自己的菜单会出现在画面里（旧实现本地菜单弹出、远端收不到右键） */
/* 右键：本地默认菜单挡掉，右键经 mouse 消息转发远端（页面自己
   的菜单出现在画面里）；同时弹观看端桥接菜单（复制/粘贴/复制
   图片等跨边界动作）。hit 为帧坐标，供边缘按坐标命中图片 */
ime.addEventListener('contextmenu', e => { e.preventDefault(); openCtx(e.clientX, e.clientY, toFrame(e)); });
/* 双击归远端页面：旧实现把输入层 dblclick 绑成观看页全屏
   切换，用户在远端页面里双击（选词/视频全屏）全被本地劫持
   （LIU 2026-11-20 指出）。现只挡默认行为；双击语义靠
   mousedown/up 的 count（MouseEvent.detail）透传到远端，
   远端视频双击就在远端页内全屏。视频元素本体也挡一道，
   防输入层放行时浏览器对 <video> 的原生双击全屏 */
ime.addEventListener('dblclick', e => { e.preventDefault(); });
vid.addEventListener('dblclick', e => { e.preventDefault(); });
function mods(e) { return (e.altKey ? 1 : 0) | (e.ctrlKey ? 2 : 0) | (e.metaKey ? 4 : 0) | (e.shiftKey ? 8 : 0); }
/* 剪贴板和弦（Ctrl/Cmd+V/C/X）不能 preventDefault：paste/copy/cut
   事件是 keydown 默认动作的一部分，挡掉后 Ctrl+V 粘贴与 Ctrl+C
   复制回读全灭（2026-11-20 实测：右键菜单粘贴可用、Ctrl+V 无效）。
   V 完全放行给 paste 事件处理；C/X 照常转发由远端执行，本地
   copy/cut 事件随后把远端剪贴板拉回本机。 */
function clipChord(e) { return (e.ctrlKey || e.metaKey) && !e.altKey && ['v', 'c', 'x'].includes(e.key.toLowerCase()); }
ime.addEventListener('keydown', e => {
  if (e.isComposing) return;
  const cc = clipChord(e);
  if (cc && e.key.toLowerCase() === 'v') return;
  if (!cc) { e.preventDefault(); ime.value = ''; }
  send({type:'key', action:'down', key: e.key, code: e.code, modifiers: mods(e)});
  /* Ctrl+C/X 照常转发远端执行复制/剪切，同时延一拍向边缘要选区
     文本回写本地剪贴板（显式模型：只在用户按键时发生，无后台
     同步、无暂存补写，不会覆盖用户在外部复制的内容） */
  if (cc && e.key.toLowerCase() !== 'v') setTimeout(requestCopy, 160);
});
ime.addEventListener('keyup', e => {
  if (e.isComposing) return;
  const cc = clipChord(e);
  if (cc && e.key.toLowerCase() === 'v') return;
  if (!cc) e.preventDefault();
  send({type:'key', action:'up', key: e.key, code: e.code, modifiers: mods(e)});
});
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
  if (!text) { showCopyBox(''); return; }
  if (navigator.clipboard && navigator.clipboard.writeText) {
    navigator.clipboard.writeText(text)
      .then(() => flashHint('已复制 ' + text.length + ' 字'))
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
      .then(() => flashHint('已复制图片'))
      .catch(() => {});
  } catch (e) {}
}
let copyBoxEl = null;
function showCopyBox(text) {
  if (!copyBoxEl) {
    copyBoxEl = document.createElement('div');
    copyBoxEl.id = 'copyBox';
    const tip = document.createElement('div');
    tip.textContent = '自动写入剪贴板不可用，请手动复制（已全选，Ctrl+C 后关闭）：';
    tip.style.marginBottom = '6px';
    const ta = document.createElement('textarea');
    ta.rows = 4; ta.style.width = '100%'; ta.readOnly = true;
    const btn = document.createElement('button');
    btn.type = 'button'; btn.textContent = '关闭';
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
let ctxEl = null, ctxImgData = '';
function closeCtx() {
  if (ctxEl) { ctxEl.remove(); ctxEl = null; }
  document.removeEventListener('pointerdown', ctxOutside, true);
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
function openCtx(cx, cy, hit) {
  closeCtx();
  ctxImgData = '';
  const menu = document.createElement('div');
  menu.id = 'ctxMenu';
  const add = (label, fn) => {
    const b = document.createElement('button');
    b.type = 'button'; b.textContent = label;
    b.onclick = () => { closeCtx(); fn(); };
    menu.appendChild(b);
  };
  add('复制', () => { if (domOn) copyMirrorSelection(); else requestCopy(); });
  add('粘贴', async () => {
    const t = await readLocalClipText();
    if (t) { send({type:'text', ...(domOn ? {dom:true} : {}), text: t}); return; }
    flashHint('请按 Ctrl+V 粘贴');
    ime.focus();
  });
  add('全选', () => sendKeyChord('a', 'KeyA'));
  add('自动模式：' + (modeAuto ? '开' : '关'), () => {
    modeAuto = !modeAuto;
    wantDomAuto = modeAuto; syncDomSub();
    if (modeAuto) { modeLock = false; flashHint('自动模式已开'); }
    else flashHint('自动模式已关');
  });
  /* 导航走 chrome 命令（双模式同一路；旧实现 DOM 模式走 nav
     消息、视频模式发 Alt+←/F5 和弦模拟，口径不一） */
  add('后退', () => chromeCmd('back'));
  add('重新加载', () => chromeCmd('reload'));
  menu.style.left = cx + 'px'; menu.style.top = cy + 'px';
  document.body.appendChild(menu);
  const r = menu.getBoundingClientRect();
  menu.style.left = Math.max(4, Math.min(cx, innerWidth - r.width - 4)) + 'px';
  menu.style.top = Math.max(4, Math.min(cy, innerHeight - r.height - 4)) + 'px';
  ctxEl = menu;
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
  b.type = 'button'; b.textContent = '复制图片'; b.setAttribute('data-act', 'copyimg');
  b.onclick = () => {
    closeCtx();
    if (!(navigator.clipboard && navigator.clipboard.write)) { flashHint('此环境不支持复制图片'); return; }
    fetch('data:image/png;base64,' + ctxImgData).then(r => r.blob()).then(blob =>
      navigator.clipboard.write([new ClipboardItem({'image/png': blob})])
        .then(() => flashHint('图片已复制')).catch(() => flashHint('复制图片失败')));
  };
  ctxEl.appendChild(b);
}
function mirrorSelText(doc) {
  const a = doc.activeElement;
  if (a && (a.tagName === 'INPUT' || a.tagName === 'TEXTAREA') && typeof a.selectionStart === 'number') {
    return a.value.substring(a.selectionStart, a.selectionEnd);
  }
  return String(doc.getSelection() || '');
}
function copyMirrorSelection() {
  const doc = mir.contentDocument;
  if (!doc) return;
  const t = mirrorSelText(doc);
  if (t) deliverLocalText(t); else requestCopy();
}

/* 分辨率跟随：把舞台区像素尺寸报给边缘（仅边缘处于「跟随窗口」
   模式时生效）。第五期起头部是文档流内的原生 DOM，舞台区就是
   远端视口的准确占位（不再手工扣条带高） */
let rzTimer = 0, rzSentW = 0, rzSentH = 0;
function stageSize() {
  const dpr = window.devicePixelRatio || 1;
  const st = document.getElementById('stage');
  return [Math.max(320, Math.round(st.clientWidth * dpr)),
          Math.max(240, Math.round(st.clientHeight * dpr))];
}
function reportSize() {
  clearTimeout(rzTimer);
  const [w, h] = stageSize();
  if (w === rzSentW && h === rzSentH) return;
  if (!rzSentW) { /* 首报立即发，边缘不必等安定窗才拿到真尺寸 */
    rzSentW = w; rzSentH = h;
    send({type:'resize', w, h});
    return;
  }
  /* 900ms 安定滞后：窗口拖动/状态切换期间尺寸反复横跳时不追
     发——远端视口、截帧基准与 RTC 编码器跟着每个中间尺寸翻
     动，正是 LIU 2026-11-20 实测「视频模式还跳」的一环（生产
     日志两尺寸交替重建编码器）；尺寸安定下来才报一次 */
  rzTimer = setTimeout(() => {
    const [w2, h2] = stageSize();
    if (w2 === rzSentW && h2 === rzSentH) return;
    rzSentW = w2; rzSentH = h2;
    send({type:'resize', w: w2, h: h2});
  }, 900);
}
addEventListener('resize', reportSize);
reportSize();
/* WebRTC 主路（Phase 1c；音频与优先级按 LIU 2026-11-19 口径修订）：
   WS 开后即尝试 rtc-start，连通后 video 显示 RTC 流（服务端同步停发
   本连接的 WS 帧）；失败/断开自动回退 canvas。AV 呈现 WebRTC 优先
   （视频+Opus 音频），HLS（ffmpeg 合流）为最终兜底，见 pollAv。 */
let pc = null, rtcOn = false, rtcTimer = 0;
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
  } else if (avOn || rtcOn) {
    ime.style.pointerEvents = '';
    vid.style.display = 'block'; cv.style.display = 'none';
    mir.style.display = 'none'; dpill.style.display = 'none';
  } else {
    ime.style.pointerEvents = '';
    vid.style.display = 'none'; cv.style.display = 'block';
    mir.style.display = 'none'; dpill.style.display = 'none';
  }
  document.getElementById('domBtn').classList.toggle('on', domOn);
  /* 静音态收敛到显示面：DOM 面显示 → 只有 RTC 音频保留时不静音
     （U01：RTC 音轨与画面解耦，DOM 藏画面仍可听声；HLS 音频封在
     流里、无独立声轨，DOM 下仍静音并明示）；
     视频面显示且影音在播（avVia 已定型）→ 解静音。此前静音标志
     散落在 pollAv/domStart 的迁移分支里，DOM→视频往返后 avVia
     已定型、没有任何分支再触发解静音，vid 恒停在 ontrack 的初始
     静音上（有声轨、无声、标签页无喇叭图标——LIU 2026-11-20
     实测无声定因） */
  if (domOn) vid.muted = !(domAudio && avVia === 'rtc');
  else if (avVia) vid.muted = false;
  placeGhost();
  jdSend();
}
/* F-03：向边缘报告本连接的真实显示模式——只有 canvas 分支
   （WS 帧流）在看 JPEG；RTC/HLS 视频元素与 DOM 镜像不消费
   帧流，边缘据此跳过没人看的帧广播（第五期后帧为原始
   screencast JPEG 透传，此记账决定广播与否） */
function jdSend() {
  const on = !domOn && !avOn && !rtcOn;
  if (jdSend.last === on) return;
  jdSend.last = on;
  send({type: 'jpeg-demand', on});
}
function rtcStop() {
  clearTimeout(rtcTimer);
  if (pc) { try { pc.close(); } catch (e) {} pc = null; }
  rtcOn = false;
  vid.srcObject = null;
  /* D03：RTC 会话换代时清分片重组缓存——旧会话的残缺包不得
     拼进新会话（可靠有序只在单会话内成立） */
  chunkBuf.clear(); chunkBytes = 0;
  send({type:'rtc-stop'});
  showDisplay();
}
function rtcStart() {
  if (pc || avVia === 'hls' || !window.RTCPeerConnection) return;
  pc = new RTCPeerConnection();
  /* rig-dom 数据通道：DOM 模式下快照/差分优先走这里（可靠有序），
     通道未开或断开时边缘自动回落 WS 文本，本页无感切换 */
  pc.ondatachannel = (ev) => {
    if (ev.channel.label !== 'rig-dom') return;
    ev.channel.onmessage = (e) => {
      try { __domStats.dc++; domFeed(JSON.parse(e.data)); } catch (err) {}
    };
  };
  pc.ontrack = (ev) => {
    /* 视频与音频分属不同 msid 流（rig / rig-audio），ev.streams[0]
       只是本轨道所在的那条流：整体赋值会让后到的音频事件把只含
       音频的流盖到 vid 上，视频轨被换走、画面恒黑而统计仍在解码
       （2026-11-19 局域网实测定位）。改为把每条到达的轨道累加进
       同一个本地 MediaStream。 */
    if (!vid.srcObject) vid.srcObject = new MediaStream();
    if (!vid.srcObject.getTracks().includes(ev.track)) vid.srcObject.addTrack(ev.track);
    vid.muted = true; vid.play().catch(() => {});
  };
  pc.onicecandidate = (ev) => { if (ev.candidate) send({type:'rtc-candidate', candidate: ev.candidate.toJSON()}); };
  pc.onconnectionstatechange = () => {
    if (!pc) return;
    if (pc.connectionState === 'connected') {
      clearTimeout(rtcTimer); rtcOn = true;
      /* M01 联动：RTC 连通后边缘 grace 停 HLS——若当前播的是 HLS，
         必须跟到 RTC 上，否则 HLS 停后 vid 无声无画 */
      if (avVia === 'hls') {
        vid.pause(); vid.removeAttribute('src'); vid.load();
        avVia = 'rtc'; vid.muted = false;
      }
      showDisplay();
    } else if (pc.connectionState === 'disconnected') {
      /* 给 ICE 3 秒恢复窗；服务端已恢复 WS 帧，过期未恢复则切回 canvas */
      clearTimeout(rtcTimer);
      rtcTimer = setTimeout(() => { if (pc && pc.connectionState !== 'connected') { rtcOn = false; showDisplay(); } }, 3000);
    } else if (pc.connectionState === 'failed' || pc.connectionState === 'closed') {
      rtcStop();
    }
  };
  send({type:'rtc-start'});
  /* 8 秒内没连通视为此路不通，回退 WS 并关掉会话 */
  clearTimeout(rtcTimer);
  rtcTimer = setTimeout(() => { if (!rtcOn) rtcStop(); }, 8000);
}
ws.addEventListener('open', () => rtcStart());
/* 影音流开关在边缘菜单里；本页只轮询状态。AV 路径按架构口径
   WebRTC 优先：RTC 已连通（或 5 秒内连通）就用 RTC 的视频+Opus
   音频；连不通才落 HLS（ffmpeg 合流，最终兜底）。 */
function startHls() {
  avVia = 'hls';
  rtcStop();
  vid.muted = false;
  vid.style.display = 'block'; cv.style.display = 'none';
  let hevcOk = window.MediaSource && MediaSource.isTypeSupported('video/mp4; codecs="hvc1.1.6.L93.B0, mp4a.40.2"');
  vid.src = (hevcOk ? '/stream/hevc/index.m3u8' : '/stream/h264/index.m3u8') + TQ;
  vid.play().catch(() => {});
}
async function pollAv() {
  try {
    const s = await (await fch('/channels')).json();
    /* M01/M03/U01：意图（av_wanted）与实际运行（av）分离——RTC healthy
       时 HLS 可以停着（av=false）但意图仍在。以下分支一律按意图决策，
       不再把“兜底编码被策略停掉”误判成“用户关了影音”而拆台 */
    const want = !!s.av_wanted;
    if (!want) domAvOffPending = false;
    if (want && !avOn && !domOn) {
      /* DOM 模式不再被影音抢占：DOM 与 RTC 本就共存（rig-dom
         数据通道跑在 RTC 上），旧逻辑「AV 来了退出 DOM」只剩
         误杀价值——6 秒保护窗一过，pollAv 会把用户好端端的
         DOM 会话停掉，连带边缘停掉页内观察者，增量静默死亡 */
      avOn = true;
      avWait = Date.now();
      if (rtcOn) { avVia = 'rtc'; vid.muted = false; showDisplay(); }
      else if (!pc) rtcStart();
    } else if (want && avOn && !avVia) {
      if (rtcOn) { avVia = 'rtc'; vid.muted = false; showDisplay(); }
      else if (Date.now() - avWait > 5000) startHls();
    } else if (!want && avOn) {
      avOn = false;
      if (avVia === 'hls') {
        vid.pause(); vid.removeAttribute('src'); vid.load();
      } else if (avVia === 'rtc') {
        vid.muted = true; /* 回到无声的 RTC 视频呈现 */
      }
      avVia = null;
      showDisplay();
      if (!pc) rtcStart();
    } else if (!want && !avOn && !domOn && !domAvOffPending && s.av_available) {
      /* 影音被关掉后无人恢复的兜底自愈：关 AV 只有 domStart
         一条路（菜单里根本没有 AV 开关），正常退出 DOM 时
         restoreAvAfterDom 会开回来；但旧版观看页、标签页在
         DOM 期间被关掉、bfcache 恢复的页面都不会跑那段恢复
         ——边缘 AV 就此恒关，pollAv 旧逻辑只对 av:true 反应，
         用户在界面上没有任何办法把声音开回来（2026-11-20
         LIU 第六轮实测：生产 /channels 恒 av:false）。视频
         模式下见到 av 关着且可用，就直接开回去；DOM 模式
         由 domOn/domAvOffPending 两道闸守住，不与互斥打架 */
      fch('/channels', {method:'POST', headers:{'Content-Type':'application/json'}, body: JSON.stringify({av:true})}).catch(() => {});
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
  dpill.style.display = (domOn && t && t !== 'DOM 镜像中') ? 'block' : 'none';
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
  const doc = mir.contentDocument;
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
/* 进 DOM 时关影音的 POST 尚未在边缘落定的窗口标记：pollAv 在此
   期间读到的 av:true 是旧状态，不能当作"菜单开了影音"而退出 DOM
   （竞态实测：DOM 刚进即被自己的 pollAv 杀掉） */
let domAvOffPending = false;
let mirMap = new Map();
/* ── F-08 模式自适应 ──────────────────────────────────────
   默认开启：连接建立即起一个 DOM 感知会话（只收快照/差分、
   不切换显示），每张快照按内容构成裁决显示模式——视频/大
   区域/大画布为主 → 视频模式；其余（表单、文章、后台页）
   → DOM 模式。用户手动切换（domBtn/退出）即锁定，锁定到
   下一次导航（快照 URL 变化）为止；右键菜单可关自动模式 */
let modeAuto = true, modeLock = false, lastSnapUrl = '';
/* U03 自适应迟滞：临界比例 + 单快照抖动曾导致视频/DOM 反复横跳
   （伴随 AV 开关）。自动裁决需连续 2 次一致、且距上次自动切换
   超过 3s 才执行；手动锁定仍是最高优先级 */
let autoWant = null, autoStable = 0, lastAutoSwitch = 0;
const AUTO_STABLE_N = 2, AUTO_MIN_INTERVAL_MS = 3000;
function snapStats(tree, vw, vh) {
  let region = 0, video = 0, canvas = 0, interactive = 0;
  const walk = (n) => {
    if (!n || n.t !== undefined) return;
    const area = n.rect ? n.rect[0] * n.rect[1] : 0;
    if (n.g === 'VIDEO') { region += area; video += area; }
    else if (n.g === 'CANVAS') canvas += area;
    else if (n.x) region += area;
    if (['INPUT', 'TEXTAREA', 'SELECT', 'BUTTON', 'A'].includes(n.g)) interactive++;
    for (const c of (n.c || [])) walk(c);
  };
  walk(tree);
  const vp = Math.max(1, (vw || 1280) * (vh || 800));
  return { region: region / vp, video: video / vp, canvas: canvas / vp, interactive };
}
function onSnapAuto(m) {
  if (m.url) {
    if (lastSnapUrl && m.url !== lastSnapUrl) {
      modeLock = false; /* 导航解锁 */
      autoWant = null; autoStable = 0; /* 导航清零迟滞计数 */
    }
    lastSnapUrl = m.url;
  }
  if (!modeAuto || modeLock || !m.tree) return;
  const st = snapStats(m.tree, m.w, m.h);
  /* 判据口径（dbg35 实测校准）：纯面积阈值分不开「表单页嵌
     小视频」（≈9%）与「视频页」（≈16%）——太近。改组合判据：
     视频面积 >10% 且页面几乎没有可交互元素（<3）才算视频页；
     表单页靠交互密度被救回 DOM，视频页交互稀少维持视频 */
  const wantDom = !((st.video > 0.10 && st.interactive < 3) || st.region > 0.30 || st.canvas > 0.20);
  window.__rigAuto = { ...st, wantDom, url: m.url || '' };
  if (wantDom !== autoWant) { autoWant = wantDom; autoStable = 1; }
  else autoStable++;
  if (autoStable < AUTO_STABLE_N) return; /* 单次快照不翻转 */
  if (Date.now() - lastAutoSwitch < AUTO_MIN_INTERVAL_MS) return;
  if (wantDom && !domOn) {
    lastAutoSwitch = Date.now();
    domStart(true); flashHint('自动模式：已切换到 DOM');
  }
  else if (!wantDom && domOn) {
    lastAutoSwitch = Date.now();
    exitDomDisplay(); flashHint('自动模式：已切换到视频');
  }
}
/* 只退显示、不停会话（感知会话留着等下一次导航快照再裁决） */
/* 退出 DOM 时恢复进 DOM 前的影音开关（domStart 关掉的那次）：
   本地 avOn 保持 false，pollAv 下一轮看到边缘 av=true 会走
   既有分支把 avVia 定型并解静音 */
function restoreAvAfterDom() {
  if (!avWasOn) return;
  avWasOn = false;
  fch('/channels', {method:'POST', headers:{'Content-Type':'application/json'}, body: JSON.stringify({av:true})}).catch(() => {});
}
function exitDomDisplay() {
  if (!domOn) return;
  domOn = false;
  wantDomShow = false; syncDomSub(); /* 自动感知订阅保留，见 syncDomSub */  mirFocusId = 0; mirFocusSel = null; mirFocusSig = null;
  domAvOffPending = false;
  hideGhost();
  clearTimeout(domTimer);
  restoreAvAfterDom();
  showDisplay();
  reportSize();
}
/* D01：DOM 订阅由“想要方”与“已订阅”两层状态收敛——后台自动感知
   与前台显示共用一份订阅，重复进入只发一次 START，一方退出而
   另一方仍需要时不发 STOP。服务端同连接二次幂等兜底。 */
let domSub = false, wantDomShow = false, wantDomAuto = false;
function syncDomSub() {
  const want = wantDomShow || wantDomAuto;
  if (want && !domSub) { domSub = true; send({type:'dom-start'}); }
  else if (!want && domSub) { domSub = false; send({type:'dom-stop'}); }
}
function domStart(auto) {
  if (!auto) modeLock = true; /* 手动进入 = 锁定到下次导航 */
  if (avOn) {
    if (avVia === 'rtc') {
      /* U01：RTC 音频与画面解耦——DOM 只藏画面不掐声。不断边缘 AV
         意图（钉住 av:true，RTC 连通时 HLS 本就不会起），退出 DOM
         时无需恢复；vid 由 showDisplay 按 domAudio 规则收敛 */
      domAudio = true;
      fch('/channels', {method:'POST', headers:{'Content-Type':'application/json'}, body: JSON.stringify({av:true})}).catch(() => {});
    } else {
      /* 影音与 DOM 互斥，但用户点了 DOM 即以 DOM 为准：关影音再进
         （pollAv 反向同理，菜单开影音会退出 DOM）。旧实现静默拒绝，
         且提示写在视频模式下不可见的 dbar 里——点了等于没反应
         （2026-11-20 实测定位）。关之前记下开着，退出时恢复。
         HLS 音频封在合流里、无独立声轨，DOM 下暂无声音（明示）。 */
      avWasOn = true;
      fch('/channels', {method:'POST', headers:{'Content-Type':'application/json'}, body: JSON.stringify({av:false})}).catch(() => {});
      domAvOffPending = true;
      setTimeout(() => { domAvOffPending = false; }, 6000);
      avOn = false;
      if (avVia === 'hls') {
        vid.pause(); vid.removeAttribute('src'); vid.load();
        flashHint('HLS 兜底下 DOM 暂无声音（切视频模式听声）');
      }
      avVia = null;
    }
  }
  /* 不停 RTC：它现在是 DOM 的优先传输（rig-dom 数据通道）；
     还没连就顺带拉起，数据先走 WS、通道开后无缝迁移 */
  if (!pc) rtcStart();
  domOn = true; domSeq = 0; mirMap = new Map();
  wantDomShow = true; syncDomSub();
  showDisplay();
  reportSize();
  setPill('等待快照…');
  clearTimeout(domTimer);
  domTimer = setTimeout(() => { if (domOn && !mirMap.size) domFallback('快照超时'); }, 8000);
}
function domStop(silent) {
  if (!domOn) return;
  if (!silent) modeLock = true; /* 手动退出 = 锁定；静默回退（出错）不锁 */
  domOn = false;
  mirFocusId = 0; mirFocusSel = null; mirFocusSig = null;
  domAvOffPending = false;
  hideGhost();
  clearTimeout(domTimer);
  wantDomShow = false; syncDomSub();
  restoreAvAfterDom();
  showDisplay();
  reportSize();
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
  const useVid = avOn || rtcOn;
  const el = useVid ? vid : cv;
  const r = el.getBoundingClientRect();
  const iw = useVid ? (vid.videoWidth || frameW) : frameW;
  const ih = useVid ? (vid.videoHeight || frameH) : frameH;
  return { x: r.left + x * r.width / Math.max(1, iw), y: r.top + y * r.height / Math.max(1, ih) };
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
  setPill('DOM 镜像不可用：' + why);
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
  if ((n.g === 'IFRAME' && n.x) || n.g === 'VIDEO') {
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
    // 同源 iframe 内容：把序列化树建成该 iframe 自身文档的根
    try {
      const idoc = el.contentDocument;
      if (idoc) idoc.replaceChild(buildNode(n.f, idoc), idoc.documentElement);
    } catch (e) {}
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
  mirVW = m.w || 0; mirVH = m.h || 0;
  layoutMirror();
  syncAddr(m.url || '');
  attachMirrorInput(doc);
  restoreMirFocus();
  if (mir.contentWindow) mir.contentWindow.scrollTo(m.sx || 0, m.sy || 0);
  placeGhost();
  domSeq = m.seq; domResyncBusy = false;
  clearTimeout(domTimer);
  setPill('DOM 镜像中');
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
        }
        if (node.parentNode) node.parentNode.removeChild(node);
      }
      mirMap.delete(op.i);
    } else if (op.o === 'add') {
      const p = mirMap.get(op.p);
      if (p) {
        const nn = buildNode(op.n, doc);
        const before = op.b ? mirMap.get(op.b) : null;
        p.insertBefore(nn, before || null);
      }
    } else if (op.o === 'at' && node && node.setAttribute) {
      if (op.v === null || op.v === undefined) node.removeAttribute(op.n);
      else node.setAttribute(op.n, op.v);
    } else if (op.o === 'tx' && node) {
      node.data = op.v;
    } else if (op.o === 'val' && node) {
      if (doc.activeElement !== node) {
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
  const pos = (e) => ({ x: Math.round(e.clientX), y: Math.round(e.clientY) });
  const editable = (t) => t && (t.tagName === 'INPUT' || t.tagName === 'TEXTAREA' || t.tagName === 'SELECT' || t.isContentEditable);
  /* 焦点记忆：focusin 记下可编辑目标的 rig id（mirMap 反查，
     焦点事件低频、线性扫可接受），selectionchange 跟踪输入
     选区；恢复动作在 restoreMirFocus（快照/增量后调用） */
  doc.addEventListener('focusin', (e) => {
    if (editable(e.target)) {
      mirFocusId = 0;
      for (const [i, n] of mirMap) { if (n === e.target) { mirFocusId = i; break; } }
      mirFocusSel = null;
      mirFocusSig = { tag: e.target.tagName, idAttr: e.target.id || '' };
    } else { mirFocusId = 0; mirFocusSig = null; }
  }, cap);
  doc.addEventListener('selectionchange', () => {
    const a = doc.activeElement;
    if (a && (a.tagName === 'INPUT' || a.tagName === 'TEXTAREA') && typeof a.selectionStart === 'number') {
      mirFocusSel = [a.selectionStart, a.selectionEnd];
    }
  });
  doc.addEventListener('mousedown', (e) => { send({type:'mouse', action:'down', dom:true, button: e.button, buttons: e.buttons, count: e.detail, ...pos(e)}); }, cap);
  doc.addEventListener('mouseup', (e) => { send({type:'mouse', action:'up', dom:true, button: e.button, buttons: e.buttons, count: e.detail, ...pos(e)}); }, cap);
  let lm = 0;
  doc.addEventListener('mousemove', (e) => {
    const n = Date.now();
    if (n - lm > 40) { lm = n; send({type:'mouse', action:'move', dom:true, buttons: e.buttons, ...pos(e)}); }
  }, cap);
  doc.addEventListener('wheel', (e) => {
    e.preventDefault();
    send({type:'wheel', dom:true, dx: Math.round(e.deltaX), dy: Math.round(e.deltaY), ...pos(e)});
  }, { capture: true, passive: false });
  doc.addEventListener('keydown', (e) => {
    if (e.isComposing) return;
    /* Ctrl+V 不转发：粘贴内容以本地剪贴板为准，由 paste 事件带
       clipboardData 直送（与视频模式同一口径，转发了会双重粘贴）。
       Ctrl+C/X 不转发、在本地兑现：镜像选区就是用户选的内容，
       远端没有对应选区，转发只会误剪远端旧选区。不压默认动作，
       copy/cut 事件随后触发、由其处理器以镜像选区写本地剪贴板 */
    if ((e.ctrlKey || e.metaKey) && e.key.toLowerCase() === 'v') return;
    const chord = (e.ctrlKey || e.metaKey) && ['c', 'x'].includes(e.key.toLowerCase());
    if (chord) return;
    if (!editable(e.target)) e.preventDefault();
    send({type:'key', dom:true, action:'down', key: e.key, code: e.code, modifiers: mods(e)});
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
  doc.addEventListener('keyup', (e) => {
    if (e.isComposing) return;
    if ((e.ctrlKey || e.metaKey) && ['c', 'x', 'v'].includes(e.key.toLowerCase())) return;
    if (!editable(e.target)) e.preventDefault();
    send({type:'key', dom:true, action:'up', key: e.key, code: e.code, modifiers: mods(e)});
  }, cap);
  doc.addEventListener('compositionend', (e) => { if (e.data) send({type:'text', dom:true, text: e.data}); }, cap);
  doc.addEventListener('click', (e) => {
    const a = e.target && e.target.closest ? e.target.closest('a') : null;
    if (a) e.preventDefault();
  }, cap);
  /* 镜像内右键：事件不冒泡到父文档，在此就地开观看端菜单——
     菜单位置换算到观看页客户区；图片命中按节点 id 报给边缘
     （镜像布局会漂移，坐标命中不可靠，按 id 永远准确） */
  doc.addEventListener('contextmenu', (e) => {
    e.preventDefault();
    const r = mir.getBoundingClientRect();
    let hit = null;
    const host = e.target && e.target.closest ? e.target.closest('[data-rig-region]') : null;
    if (host) hit = { kind: 'node', i: +host.getAttribute('data-rig-region') };
    else {
      let n = e.target;
      while (n && !hit) {
        for (const [i, el] of mirMap) { if (el === n) { hit = { kind: 'node', i }; break; } }
        n = n.parentNode;
      }
    }
    const k = mirVW ? r.width / mirVW : 1;
    openCtx(Math.round(r.left + e.clientX * k), Math.round(r.top + e.clientY * k), hit);
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

function chromeCmd(action, extra) { send({ type: 'chrome', action, ...(extra || {}) }); }
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
    ttl.textContent = t.title || t.url || '新标签页';
    const cls = document.createElement('button');
    cls.className = 'cls';
    cls.title = '关闭标签页';
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
  hdrState = m;
  window.__rigChrome = m;
  renderTabs(m);
  backBtn.disabled = !m.canBack;
  fwdBtn.disabled = !m.canFwd;
  reloadBtn.replaceChildren(svgIcon(m.loading ? ICO.stop : ICO.reload));
  reloadBtn.title = m.loading ? '停止加载' : '重新加载';
  loadbar.classList.toggle('go', !!m.loading);
  const at = (m.tabs || [])[m.active || 0];
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
  if (e.key === 'Enter') {
    chromeCmd('navigate', { url: addrEl.value });
    addrEl.blur();
    ime.focus();
  } else if (e.key === 'Escape') {
    const at = hdrState && (hdrState.tabs || [])[hdrState.active || 0];
    addrEl.value = at ? at.url : '';
    addrEl.blur();
    ime.focus();
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
  item('在页面中查找', openFind, { sc: 'Ctrl+F' });
  item('新建标签页', () => chromeCmd('new-tab'));
  item('下载', () => chromeCmd('open-internal', { url: 'chrome://downloads' }));
  item('扩展', () => chromeCmd('open-internal', { url: 'chrome://extensions' }));
  item('历史记录', () => chromeCmd('open-internal', { url: 'chrome://history' }));
  item('设置', () => chromeCmd('open-internal', { url: 'chrome://settings' }));
  item('打印…', () => chromeCmd('print'));
  sep();
  item('分辨率：跟随窗口', () => chromeCmd('res-auto'), { checked: st.res === 'auto' });
  item('分辨率：1280 × 800', () => chromeCmd('res-fixed', { w: 1280, h: 800 }), { checked: st.res === '1280x800' });
  item('分辨率：1920 × 1080', () => chromeCmd('res-fixed', { w: 1920, h: 1080 }), { checked: st.res === '1920x1080' });
  sep();
  item('AI 通道', () => chromeCmd('toggle-ai'), { checked: !!st.gateAi });
  item('人通道', () => chromeCmd('toggle-human'), { checked: !!st.gateHuman });
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
  ime.focus();
}
let findTextTimer = 0;
findInput.addEventListener('input', () => {
  clearTimeout(findTextTimer);
  findTextTimer = setTimeout(() => chromeCmd('find-text', { text: findInput.value }), 150);
});
findInput.addEventListener('keydown', (e) => {
  e.stopPropagation();
  if (e.key === 'Enter') chromeCmd(e.shiftKey ? 'find-prev' : 'find-next');
  else if (e.key === 'Escape') closeFind();
});
findInput.addEventListener('mousedown', (e) => e.stopPropagation());
document.getElementById('findNext').onclick = () => chromeCmd('find-next');
document.getElementById('findPrev').onclick = () => chromeCmd('find-prev');
document.getElementById('findClose').onclick = closeFind;
/* ── 全局快捷键（Chrome 口径）：Ctrl/Cmd+F 查找、Ctrl/Cmd+L
   地址栏。捕获阶段先于 ime 的转发处理拦截；焦点已在工具栏
   输入框里时不重复处理（它们各自 stopPropagation） ── */
document.addEventListener('keydown', (e) => {
  if (!(e.ctrlKey || e.metaKey) || e.altKey) return;
  const k = e.key.toLowerCase();
  if (k === 'f') { e.preventDefault(); e.stopPropagation(); openFind(); }
  else if (k === 'l') { e.preventDefault(); e.stopPropagation(); addrEl.focus(); addrEl.select(); }
}, true);
if (new URLSearchParams(location.search).get('dom') === '1') {
  ws.addEventListener('open', () => domStart());
}
/* F-08：自动模式默认起感知会话（快照到达后由 onSnapAuto 裁决
   显示模式；视频页保持视频、表单/文章页自动进 DOM） */
ws.addEventListener('open', () => { if (modeAuto && !wantDomAuto) { wantDomAuto = true; syncDomSub(); } });
pollAv();
ime.focus();
