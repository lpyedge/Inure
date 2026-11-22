#!/usr/bin/env bash
# 清理边缘程序在 /tmp 留下的残余临时目录（SIGKILL/崩溃的遗留）。
# 用法：scripts/clean-tmp.sh [--yes]     默认只列出（dry run），加 --yes 才删除
# 判据：目录名里的 pid 对应的进程已不存在才算残余；仍在运行的实例的目录不动。
# 覆盖：/tmp/rig-edge-<pid>、/tmp/rig-edge-<pid>-uploads、/tmp/rig-edge-hls-<pid>
set -euo pipefail
TMP="${TMPDIR:-/tmp}"
YES=0; [ "${1:-}" = "--yes" ] && YES=1
n=0
for d in "$TMP"/rig-edge-[0-9]* "$TMP"/rig-edge-hls-[0-9]*; do
  [ -d "$d" ] || continue
  pid="$(basename "$d" | grep -oE '[0-9]+' | head -1)"
  if kill -0 "$pid" 2>/dev/null; then
    echo "保留（进程 $pid 仍在运行）：$d"; continue
  fi
  n=$((n+1))
  if [ "$YES" = 1 ]; then rm -rf -- "$d"; echo "已删除：$d"; else echo "残余：$d"; fi
done
if [ "$YES" = 1 ]; then echo "共删除 $n 项"; else echo "共 $n 项残余（加 --yes 删除）"; fi
