#!/usr/bin/env bash
# 局域网测试启动：便携 PulseAudio（.temp/pulseaudio）+ rigsink 虚拟声卡 + edge。
# 用法：bash scripts/edge-lan.sh [TOKEN] [PORT]
#   TOKEN 缺省随机生成；PORT 缺省 39871。
# 停止：kill <edge-pid> <pa-pid>（脚本退出时打印）。
set -euo pipefail
cd "$(dirname "$0")/.."
ROOT="$PWD"

TOKEN="${1:-$(head -c 16 /dev/urandom | od -An -tx1 | tr -d ' \n')}"
PORT="${2:-39871}"
IP="$(ip -4 addr show scope global | grep -oP '(?<=inet\s)\d+(\.\d+){3}' | grep -v '^172\.' | head -1)"
[ -n "$IP" ] || { echo "找不到局域网 IP"; exit 1; }

PA="$ROOT/.temp/pulseaudio"
RUN="$ROOT/.temp/pulse-run"
HOME_DIR="$ROOT/.temp/pulse-home"
[ -x "$PA/usr/bin/pulseaudio" ] || { echo "$PA 缺 pulseaudio，先解包 deb"; exit 1; }
mkdir -p "$RUN" "$HOME_DIR"
chmod 700 "$RUN" "$HOME_DIR"

export PATH="$PA/usr/bin:$PATH"
export LD_LIBRARY_PATH="$PA/usr/lib/x86_64-linux-gnu:$PA/usr/lib/x86_64-linux-gnu/pulseaudio:$PA/usr/lib/pulse-17.0+dfsg1/modules${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}"
export XDG_RUNTIME_DIR="$RUN"
export PULSE_RUNTIME_PATH="$RUN"
export HOME="$HOME_DIR"
export DBUS_SESSION_BUS_ADDRESS="unix:path=/nonexistent"
export DBUS_SYSTEM_BUS_ADDRESS="unix:path=/nonexistent"
SOCK="$RUN/native"
export PULSE_SERVER="unix:$SOCK"
export PULSE_SINK="rigsink"

if [ ! -S "$SOCK" ]; then
  "$PA/usr/bin/pulseaudio" \
    --daemonize=no --exit-idle-time=-1 --disallow-exit -n \
    "--dl-search-path=$PA/usr/lib/pulse-17.0+dfsg1/modules" \
    --load=module-native-protocol-unix \
    --load="module-null-sink sink_name=rigsink" \
    >"$RUN/daemon.log" 2>&1 &
  echo $! >"$RUN/daemon.pid"
  for i in $(seq 1 100); do
    [ -S "$SOCK" ] && break
    sleep 0.1
  done
  [ -S "$SOCK" ] || { echo "pulseaudio 未起来，看 $RUN/daemon.log"; exit 1; }
fi
"$PA/usr/bin/pactl" set-default-sink rigsink >/dev/null 2>&1 || true

# 优先 release：debug 构建下 DOM 序列化/JPEG/合成慢一个量级（实测 DOM 模式明显卡顿）
BIN="$ROOT/.temp/target/release/rig-edge-browser"
[ -x "$BIN" ] || BIN="$ROOT/.temp/target/debug/rig-edge-browser"
[ -x "$BIN" ] || { echo "先 cargo build --release"; exit 1; }
RIG_EDGE_TOKEN="$TOKEN" PULSE_SERVER="$PULSE_SERVER" PULSE_SINK=rigsink \
  "$BIN" --listen "$IP:$PORT" --human --av \
  >"$ROOT/.temp/edge-lan.log" 2>&1 &
echo $! >"$ROOT/.temp/edge-lan.pid"

sleep 2
if curl -sk "https://$IP:$PORT/health" | grep -q '"ok":true'; then
  echo "OK  pid=$(cat "$ROOT/.temp/edge-lan.pid") pa_pid=$(cat "$RUN/daemon.pid" 2>/dev/null)"
  echo "URL: https://$IP:$PORT/?token=$TOKEN"
  echo "停：kill $(cat "$ROOT/.temp/edge-lan.pid") $(cat "$RUN/daemon.pid" 2>/dev/null)"
else
  echo "edge 未就绪，看 $ROOT/.temp/edge-lan.log"
  tail -20 "$ROOT/.temp/edge-lan.log"
  exit 1
fi
