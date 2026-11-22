#!/usr/bin/env bash
# 清理 Linux 上边缘程序在 TMPDIR（默认 /tmp）下残留的临时目录。
# 用法：scripts/clean-tmp.sh [--yes]     默认只列出（dry run），加 --yes 才删除
# 判据：目录名里的 pid 对应的进程已不存在才算残余；仍在运行的实例的目录不动。
# 覆盖：rig-edge-<pid>、rig-edge-<pid>-uploads、rig-edge-<pid>-pulse、rig-edge-hls-<pid>。
set -euo pipefail
TMP="${TMPDIR:-/tmp}"
case "$#:${1:-}" in
  0:) YES=0 ;;
  1:--yes) YES=1 ;;
  *) echo "Usage: $0 [--yes]" >&2; exit 2 ;;
esac
if [ ! -d "/proc/$$" ]; then
  echo "Temporary directory cleanup requires Linux /proc for process checks." >&2
  exit 1
fi
n=0
for d in "$TMP"/rig-edge-[0-9]* "$TMP"/rig-edge-hls-[0-9]*; do
  [[ -d "$d" && ! -L "$d" && -O "$d" ]] || continue
  name="${d##*/}"
  if [[ "$name" =~ ^rig-edge-([0-9]+)(-(uploads|pulse))?$ || "$name" =~ ^rig-edge-hls-([0-9]+)$ ]]; then
    pid="${BASH_REMATCH[1]}"
  else
    continue
  fi
  if [ -d "/proc/$pid" ]; then
    echo "Keeping (process $pid is still running): $d"; continue
  fi
  n=$((n+1))
  if [ "$YES" = 1 ]; then rm -rf -- "$d"; echo "Deleted: $d"; else echo "Leftover: $d"; fi
done
if [ "$YES" = 1 ]; then echo "Deleted $n item(s)"; else echo "$n leftover item(s) (add --yes to delete)"; fi
