/* 观看页鉴权引导（docs/HARDENING_PLAN_2026-11-22.md §二）。
   启动令牌只在 POST /session 的 body 里出现，不进 URL；之后用
   access 票据（Authorization 头 / WS 首帧）与 refresh 静默续期。
   独立打开：显示令牌输入页；被宿主页面内嵌：宿主 postMessage
   {type:'rig-auth', token} 自动授权（来源由服务端 frame-ancestors
   白名单保证，这里只认 window.parent 发来的消息）。
   授权成功后才加载 viewer.js。 */
(() => {
  const A = { access: '', refresh: '', streamKey: '', timer: 0, started: false };

  async function post(path, body) {
    const r = await fetch(path, {
      method: 'POST', headers: { 'Content-Type': 'application/json' }, body: JSON.stringify(body),
    });
    if (!r.ok) throw Object.assign(new Error('http ' + r.status), { status: r.status });
    return r.json();
  }

  function apply(p) {
    A.access = p.access; A.refresh = p.refresh; A.streamKey = p.stream_key;
    clearTimeout(A.timer);
    A.timer = setTimeout(renew, Math.max(10, p.expires_in - 60) * 1000);
  }

  /* refresh 每次使用即轮换；401 = 边缘已重启（票据全部失效），回到输入页 */
  async function renew() {
    try {
      apply(await post('/session/refresh', { refresh: A.refresh }));
    } catch (e) {
      if (e.status === 401) { location.reload(); return; }
      A.timer = setTimeout(renew, 5000);
    }
  }

  /* 带 access 头的 fetch；401 时续期后重试一次 */
  async function authFetch(u, o = {}) {
    const go = () => fetch(u, { ...o, headers: { ...(o.headers || {}), Authorization: 'Bearer ' + A.access } });
    let r = await go();
    if (r.status === 401) { await renew(); r = await go(); }
    return r;
  }

  /* 浏览器 WebSocket 无法设请求头：先注册首帧认证监听，再交给调用方，
     保证 auth 是该连接发出的第一帧 */
  function openWs() {
    const ws = new WebSocket((location.protocol === 'https:' ? 'wss://' : 'ws://') + location.host + '/ws');
    ws.addEventListener('open', () => ws.send(JSON.stringify({ type: 'auth', access: A.access })));
    return ws;
  }

  async function start(token) {
    if (A.started) return;
    apply(await post('/session', { token }));
    A.started = true;
    document.getElementById('rigLogin')?.remove();
    const s = document.createElement('script');
    s.src = '/viewer.js';
    document.body.appendChild(s);
  }

  window.__rigAuth = { fetch: authFetch, openWs, streamKey: () => A.streamKey };

  const box = document.createElement('div');
  box.id = 'rigLogin';
  box.style.cssText = 'position:fixed;inset:0;z-index:99999;display:flex;align-items:center;justify-content:center;background:#111;color:#eee;font:14px system-ui,sans-serif';
  box.innerHTML = '<form style="display:flex;flex-direction:column;gap:10px;width:min(320px,90vw)">' +
    '<div>请输入访问令牌</div>' +
    '<input type="password" autocomplete="off" style="padding:8px;font-size:14px">' +
    '<button type="submit" style="padding:8px">进入</button>' +
    '<div class="msg" style="color:#f88;min-height:1.2em"></div></form>';
  document.body.appendChild(box);
  const form = box.querySelector('form'), input = box.querySelector('input'), msg = box.querySelector('.msg');
  const attempt = async (token) => {
    try { await start(token); }
    catch (e) { msg.textContent = e.status === 401 ? '令牌无效' : '连接失败'; }
  };
  form.addEventListener('submit', (ev) => { ev.preventDefault(); attempt(input.value); });
  input.focus();

  if (window.parent !== window) {
    addEventListener('message', (ev) => {
      if (ev.source !== window.parent) return;
      const d = ev.data;
      if (d && d.type === 'rig-auth' && typeof d.token === 'string') attempt(d.token);
    });
    window.parent.postMessage({ type: 'rig-ready' }, '*');
  }
})();
