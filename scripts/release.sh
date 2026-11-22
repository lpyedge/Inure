#!/usr/bin/env bash
# 运行当前平台的发布构建，输出 full/slim ZIP 和依赖索引。
set -euo pipefail
cd "$(dirname "$0")/.."
ROOT="$PWD"
APP_VERSION="$(awk -F'"' '/^version *=/ {print $2; exit}' Cargo.toml)"
GIT="$(git rev-parse --short HEAD)"
[ -z "$(git status --porcelain)" ] || GIT="$GIT-dirty"
case "$(uname -s):$(uname -m)" in
  Linux:x86_64) TARGET=linux-x86_64; TRIPLET=x86_64-linux-gnu ;;
  Linux:aarch64) TARGET=linux-arm64; TRIPLET=aarch64-linux-gnu ;;
  Darwin:arm64) TARGET=macos-arm64 ;;
  MINGW*:*|MSYS*:*|CYGWIN*:*)
    TARGET="$(python -c 'import platform; print("windows-arm64" if platform.machine().lower() == "arm64" else "windows-x64")')" ;;
  *) echo "Unsupported native release host." >&2; exit 1 ;;
esac
if [ -n "${RIG_RELEASE_TARGET:-}" ] && [ "$TARGET" != "$RIG_RELEASE_TARGET" ]; then
  echo "Expected a native $RIG_RELEASE_TARGET build host, detected $TARGET." >&2
  exit 1
fi
mkdir -p .temp dist
STAGE="$(mktemp -d "$ROOT/.temp/release.XXXXXX")"
trap 'rm -rf "$STAGE"' EXIT

if [[ "$TARGET" == linux-* ]]; then
  scripts/prepare-media.sh
  MEDIA_ROOT="$(realpath "${RIG_MEDIA_DIR:-$ROOT/.temp/media-deps}/root")"
  MEDIA_LIB="$MEDIA_ROOT/usr/lib/$TRIPLET"
  # Ubuntu 24.04 的 BLAS/LAPACK 解包在各自目录，系统 alternatives 只在安装时建立。
  MEDIA_LD="$MEDIA_LIB:$MEDIA_LIB/pulseaudio:$MEDIA_LIB/blas:$MEDIA_LIB/lapack"
  env -u FFMPEG_DIR -u PKG_CONFIG_PATH \
    PKG_CONFIG_SYSROOT_DIR="$MEDIA_ROOT" PKG_CONFIG_LIBDIR="$MEDIA_LIB/pkgconfig" \
    LD_LIBRARY_PATH="$MEDIA_LD" bash scripts/build.sh release
  cargo build --locked --release -p rig-launcher
  BIN_DIR="${CARGO_TARGET_DIR:-$ROOT/target}/release"
  mkdir -p "$STAGE/engine" "$STAGE/runtime/lib" "$STAGE/runtime/THIRD_PARTY_LICENSES"
  cp "$BIN_DIR/rig-launcher" "$STAGE/rig-edge-browser"
  cp "$BIN_DIR/rig-edge-browser" "$STAGE/engine/rig-edge-browser-core"

  is_os_baseline() {
    case "$1" in
      libc.so.*|libm.so.*|libmvec.so.*|libpthread.so.*|libdl.so.*|librt.so.*|libresolv.so.*|libutil.so.*|libanl.so.*|libBrokenLocale.so.*|libnss_*.so.*|libthread_db.so.*|ld-linux*.so*|ld64.so*) return 0 ;;
      *) return 1 ;;
    esac
  }

  copy_elf_closure() {
    local elf="$1" destination="$2" ld_path="$3" output soname source
    output="$(LC_ALL=C LD_LIBRARY_PATH="$ld_path" ldd "$elf")"
    if grep -q 'not found' <<< "$output"; then
      echo "Missing shared libraries for $elf:" >&2
      grep 'not found' <<< "$output"
      exit 1
    fi
    while IFS=$'\t' read -r soname source; do
      [ -n "$soname" ] || continue
      is_os_baseline "$soname" && continue
      source="$(realpath "$source")"
      if [[ "$source" != "$MEDIA_ROOT"/* ]]; then
        echo "Runtime library $soname resolved outside the prepared media packages: $source" >&2
        exit 1
      fi
      cp "$source" "$destination/$soname"
    done < <(awk '/=>[[:space:]]+\// {
      soname = $1
      sub(/^[[:space:]]*[^[:space:]]+[[:space:]]+=>[[:space:]]+/, "")
      sub(/[[:space:]]+\(0x[[:xdigit:]]+\)$/, "")
      print soname "\t" $0
    }' <<< "$output")
  }

  copy_elf_closure "$BIN_DIR/rig-edge-browser" "$STAGE/runtime/lib" "$MEDIA_LD"
  PA_BIN="$MEDIA_ROOT/usr/bin/pulseaudio"
  PA_MODULE_DIR="$(dirname "$(find "$MEDIA_ROOT/usr/lib" -name module-null-sink.so -print -quit)")"
  [ -f "$PA_MODULE_DIR/module-native-protocol-unix.so" ] || { echo "Missing PulseAudio modules." >&2; exit 1; }
  PA_LIB_DST="$STAGE/runtime/pulseaudio/usr/lib/$TRIPLET"
  PA_MODULE_DST="$STAGE/runtime/pulseaudio/${PA_MODULE_DIR#"$MEDIA_ROOT"/}"
  PA_BIN_DST="$STAGE/runtime/pulseaudio/usr/bin"
  mkdir -p "$PA_BIN_DST" "$PA_LIB_DST/pulseaudio" "$PA_MODULE_DST"
  cp -L "$PA_BIN" "$PA_BIN_DST/pulseaudio.bin"
  # 私有 daemon 使用独立会话总线，避免与宿主声音服务争用。
  cat > "$PA_BIN_DST/pulseaudio" <<'SH'
#!/bin/sh
set -eu
DBUS_SESSION_BUS_ADDRESS="unix:path=$PULSE_RUNTIME_PATH/dbus" exec "${0%/*}/pulseaudio.bin" "$@"
SH
  chmod +x "$PA_BIN_DST/pulseaudio"
  for module in module-native-protocol-unix.so module-null-sink.so; do
    cp -L "$PA_MODULE_DIR/$module" "$PA_MODULE_DST/"
    copy_elf_closure "$PA_MODULE_DIR/$module" "$PA_LIB_DST" "$MEDIA_LD:$PA_MODULE_DIR"
  done
  copy_elf_closure "$PA_BIN" "$PA_LIB_DST" "$MEDIA_LD:$PA_MODULE_DIR"
  while IFS= read -r notice; do
    package="$(basename "$(dirname "$notice")")"
    cp "$notice" "$STAGE/runtime/THIRD_PARTY_LICENSES/$package-copyright"
  done < <(find "$MEDIA_ROOT/usr/share/doc" -mindepth 2 -maxdepth 2 -type f -name copyright)
  . /etc/os-release
  AV_CODEC="$(PKG_CONFIG_LIBDIR="$MEDIA_LIB/pkgconfig" pkg-config --modversion libavcodec)"
  printf 'ubuntu-%s-ffmpeg-%s\n' "$VERSION_CODENAME" "${AV_CODEC%%.*}" > "$STAGE/runtime/abi.txt"
  getconf GNU_LIBC_VERSION > "$STAGE/runtime/build-libc.txt"
else
  python scripts/release-desktop.py "$STAGE"
fi

if [[ "$TARGET" == windows-* ]]; then
  PYTHON=python
  LAUNCHER="$STAGE/rig-edge-browser.exe"
else
  PYTHON=python3
  LAUNCHER="$STAGE/rig-edge-browser"
fi
"$LAUNCHER" --check-media-runtime
mkdir -p "$STAGE/docs"
cp README.md CLAUDE.md AGENTS.md "$STAGE/"
cp docs/*.md "$STAGE/docs/"
"$PYTHON" scripts/release-archive.py "$STAGE" "$TARGET" "rig-edge-browser-$APP_VERSION-$GIT"
