#!/usr/bin/env bash
# 在便携 PulseAudio（.temp/pulseaudio，不需要安装到系统）+ rigsink 虚拟声卡环境下运行命令。
# 用法：scripts/with-pulse.sh <命令...>
#   例：RIG_EDGE_IT=1 scripts/with-pulse.sh cargo test -p rig-edge-browser --test edge_it
# 只把 PulseAudio 相关环境变量作用于被运行的命令，**不改 HOME**（否则找不到 ~/.cache/ms-playwright 的 Chromium）。
# 便携 PulseAudio 的获取方式见 docs/04-依赖与环境.md §4.2。
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
PA="$ROOT/.temp/pulseaudio"
RUN="$ROOT/.temp/pulse-run"
HOME_DIR="$ROOT/.temp/pulse-home"
[ -x "$PA/usr/bin/pulseaudio" ] || { echo "缺少 $PA/usr/bin/pulseaudio：先按 docs/04 §4.2 解包 deb" >&2; exit 1; }
[ $# -gt 0 ] || { echo "用法：$0 <命令...>" >&2; exit 2; }
mkdir -p "$RUN" "$HOME_DIR"
chmod 700 "$RUN" "$HOME_DIR"

export PATH="$PA/usr/bin:$PATH"
export LD_LIBRARY_PATH="$PA/usr/lib/x86_64-linux-gnu:$PA/usr/lib/x86_64-linux-gnu/pulseaudio:$PA/usr/lib/pulse-17.0+dfsg1/modules${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}"
export XDG_RUNTIME_DIR="$RUN" PULSE_RUNTIME_PATH="$RUN"
export DBUS_SESSION_BUS_ADDRESS="unix:path=/nonexistent" DBUS_SYSTEM_BUS_ADDRESS="unix:path=/nonexistent"
SOCK="$RUN/native"
export PULSE_SERVER="unix:$SOCK" PULSE_SINK=rigsink

if [ ! -S "$SOCK" ]; then
  HOME="$HOME_DIR" "$PA/usr/bin/pulseaudio" --daemonize=no --exit-idle-time=-1 --disallow-exit -n \
    "--dl-search-path=$PA/usr/lib/pulse-17.0+dfsg1/modules" \
    --load=module-native-protocol-unix --load="module-null-sink sink_name=rigsink" \
    >"$RUN/daemon.log" 2>&1 &
  echo $! >"$RUN/daemon.pid"
  for _ in $(seq 1 100); do [ -S "$SOCK" ] && break; sleep 0.1; done
  [ -S "$SOCK" ] || { echo "pulseaudio 未起来，看 $RUN/daemon.log" >&2; exit 1; }
fi
HOME="$HOME_DIR" "$PA/usr/bin/pactl" set-default-sink rigsink >/dev/null 2>&1 || true
exec "$@"
