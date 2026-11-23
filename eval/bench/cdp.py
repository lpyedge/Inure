"""最小 CDP 辅助（只给 op.py reset 等待页面就绪用），只用标准库，不依赖 websocket-client。

`CDP` 是被测实例的 Chromium 调试端口，由调用方在使用前赋值。
WebSocket 只实现了本地 CDP 需要的部分：文本帧、分片、ping/pong、关闭。
"""
import base64
import itertools
import json
import os
import socket
import struct
import urllib.request

CDP = 0
_ids = itertools.count(1)


class WS:
    def __init__(self, url, timeout=30):
        rest = url.split("://", 1)[1]
        hostport, _, path = rest.partition("/")
        host, _, port = hostport.partition(":")
        self.sock = socket.create_connection((host, int(port or 80)), timeout=timeout)
        key = base64.b64encode(os.urandom(16)).decode()
        self.sock.sendall(
            (
                f"GET /{path} HTTP/1.1\r\nHost: {hostport}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n"
                f"Sec-WebSocket-Key: {key}\r\nSec-WebSocket-Version: 13\r\n\r\n"
            ).encode()
        )
        head = b""
        while b"\r\n\r\n" not in head:
            chunk = self.sock.recv(4096)
            if not chunk:
                raise RuntimeError("websocket handshake closed")
            head += chunk
        if b" 101 " not in head.split(b"\r\n", 1)[0]:
            raise RuntimeError("websocket handshake refused: " + head.split(b"\r\n", 1)[0].decode())
        self._buf = head.split(b"\r\n\r\n", 1)[1]

    def _read(self, n):
        while len(self._buf) < n:
            chunk = self.sock.recv(65536)
            if not chunk:
                raise RuntimeError("websocket closed")
            self._buf += chunk
        out, self._buf = self._buf[:n], self._buf[n:]
        return out

    def _send_frame(self, opcode, payload):
        n = len(payload)
        head = bytes([0x80 | opcode])
        if n < 126:
            head += bytes([0x80 | n])
        elif n < 65536:
            head += bytes([0x80 | 126]) + struct.pack(">H", n)
        else:
            head += bytes([0x80 | 127]) + struct.pack(">Q", n)
        mask = os.urandom(4)
        self.sock.sendall(head + mask + bytes(b ^ mask[i % 4] for i, b in enumerate(payload)))

    def send(self, text):
        self._send_frame(0x1, text.encode())

    def recv(self):
        message, kind = b"", None
        while True:
            b0, b1 = self._read(2)
            opcode, n = b0 & 0x0F, b1 & 0x7F
            if n == 126:
                n = struct.unpack(">H", self._read(2))[0]
            elif n == 127:
                n = struct.unpack(">Q", self._read(8))[0]
            payload = self._read(n)  # 服务端帧不加掩码
            if opcode == 0x9:
                self._send_frame(0xA, payload)
                continue
            if opcode == 0x8:
                raise RuntimeError("websocket closed by peer")
            if opcode in (0x1, 0x2):
                kind = opcode
            message += payload
            if b0 & 0x80:
                return message.decode() if kind == 0x1 else message

    def close(self):
        try:
            self._send_frame(0x8, b"")
        except OSError:
            pass
        self.sock.close()


def cdp_ws(url_filter=""):
    tabs = json.load(urllib.request.urlopen(f"http://127.0.0.1:{CDP}/json/list", timeout=10))
    tab = [t for t in tabs if t.get("type") == "page" and url_filter in t.get("url", "")]
    if not tab:
        raise RuntimeError("no page target: " + json.dumps([t.get("url") for t in tabs]))
    return WS(tab[0]["webSocketDebuggerUrl"])


def cdp_call(ws, method, params=None):
    i = next(_ids)
    ws.send(json.dumps({"id": i, "method": method, "params": params or {}}))
    while True:
        m = json.loads(ws.recv())
        if m.get("id") == i:
            if "error" in m:
                raise RuntimeError(str(m["error"]))
            return m.get("result", {})


def cdp_eval(ws, expr):
    r = cdp_call(ws, "Runtime.evaluate", {"expression": expr, "returnByValue": True})
    return r.get("result", {}).get("value")
