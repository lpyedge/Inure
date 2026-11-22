#!/usr/bin/env bash
# 发布打包：release 构建 → 收集运行库与 PulseAudio → tar.gz + sha256。
# 用法：scripts/release.sh
# 产物：dist/rig-edge-browser-<版本>-<git>-<系统>-<架构>.tar.gz 及同名 .sha256
set -euo pipefail
cd "$(dirname "$0")/.."
ROOT="$PWD"

VERSION="$(awk -F'"' '/^version *=/ {print $2; exit}' Cargo.toml)"
GIT="$(git rev-parse --short HEAD 2>/dev/null || echo nogit)"
[ -z "$(git status --porcelain 2>/dev/null)" ] || GIT="$GIT-dirty"
OS="$(uname -s | tr 'A-Z' 'a-z')"; ARCH="$(uname -m)"
NAME="rig-edge-browser-$VERSION-$GIT-$OS-$ARCH"

bash scripts/build.sh release
BIN="${CARGO_TARGET_DIR:-$ROOT/target}/release/rig-edge-browser"

mkdir -p "$ROOT/.temp" "$ROOT/dist"
SCRATCH="$(mktemp -d "$ROOT/.temp/release.XXXXXX")"
trap 'rm -rf "$SCRATCH"' EXIT
STAGE="$SCRATCH/$NAME"
mkdir -p "$STAGE/docs" "$STAGE/lib" "$STAGE/THIRD_PARTY_LICENSES"
cp "$BIN" "$STAGE/rig-edge-browser.bin"
LOADER="$(LC_ALL=C readelf -l "$BIN" | sed -n 's/.*Requesting program interpreter: \(.*\)]/\1/p')"
[ -x "$LOADER" ] || { echo "ELF interpreter not found for $BIN" >&2; exit 1; }
# 加载路径只交给边缘的 ELF 加载器，Chromium 子进程不会继承。
{
  cat <<'SH'
#!/bin/sh
set -eu
case "$0" in
  */*) parent=${0%/*} ;;
  *) parent=. ;;
esac
directory=$(CDPATH= cd -- "$parent" && pwd)
SH
  printf 'exec "%s" --library-path "$directory/lib" "$directory/rig-edge-browser.bin" "$@" --pulseaudio-dir "$directory/pulseaudio"\n' "$LOADER"
} > "$STAGE/rig-edge-browser"
chmod +x "$STAGE/rig-edge-browser"
cp README.md CLAUDE.md AGENTS.md "$STAGE/"
cp docs/*.md "$STAGE/docs/"

is_os_baseline() {
  case "$1" in
    libc.so.*|libm.so.*|libmvec.so.*|libpthread.so.*|libdl.so.*|librt.so.*|libresolv.so.*|libutil.so.*|libanl.so.*|libBrokenLocale.so.*|libnss_*.so.*|libthread_db.so.*|ld-linux*.so*|ld64.so*) return 0 ;;
    *) return 1 ;;
  esac
}

copy_host_notice() {
  local source="$1" owner notice
  owner="$(dpkg-query -S "$source" 2>/dev/null | head -n1 | sed 's/: .*//' || true)"
  owner="${owner%%:*}"
  [ -n "$owner" ] || return 0
  notice="/usr/share/doc/$owner/copyright"
  [ -f "$notice" ] || return 0
  cp -n "$notice" "$STAGE/THIRD_PARTY_LICENSES/$owner-copyright" 2>/dev/null || true
}

copy_elf_closure() {
  local elf="$1" destination="$2" ld_path="${3:-}" output
  if [ -n "$ld_path" ]; then
    output="$(LC_ALL=C LD_LIBRARY_PATH="$ld_path" ldd "$elf")"
  else
    output="$(env -u LD_LIBRARY_PATH LC_ALL=C ldd "$elf")"
  fi
  if grep -q 'not found' <<< "$output"; then
    echo "Missing shared libraries for $elf:" >&2
    grep 'not found' <<< "$output" >&2
    exit 1
  fi
  while IFS=$'\t' read -r soname source; do
    [ -n "$soname" ] || continue
    is_os_baseline "$soname" && continue
    cp -L "$source" "$destination/$soname"
    copy_host_notice "$source"
  done < <(awk '/=>[[:space:]]+\// {
    soname = $1
    sub(/^[[:space:]]*[^[:space:]]+[[:space:]]+=>[[:space:]]+/, "")
    sub(/[[:space:]]+\(0x[[:xdigit:]]+\)$/, "")
    print soname "\t" $0
  }' <<< "$output")
}

copy_elf_closure "$BIN" "$STAGE/lib"

if [ -x "$ROOT/.temp/pulseaudio/usr/bin/pulseaudio" ]; then
  PA_ROOT="$ROOT/.temp/pulseaudio"
  PA_BIN="$PA_ROOT/usr/bin/pulseaudio"
elif [ -x /usr/bin/pulseaudio ]; then
  PA_ROOT="/"
  PA_BIN=/usr/bin/pulseaudio
else
  echo "PulseAudio not found: unpack it under .temp/pulseaudio or install /usr/bin/pulseaudio" >&2
  exit 1
fi

shopt -s nullglob
PA_ARCH_DIRS=("$PA_ROOT"/usr/lib/*/pulseaudio)
PA_MODULE_DIRS=("$PA_ROOT"/usr/lib/pulse-*/modules)
PA_ARCH_DIR=""
for dir in "${PA_ARCH_DIRS[@]}"; do
  if [ -d "$dir" ]; then PA_ARCH_DIR="$dir"; break; fi
done
PA_MODULE_DIR=""
for dir in "${PA_MODULE_DIRS[@]}"; do
  if [ -f "$dir/module-native-protocol-unix.so" ] && [ -f "$dir/module-null-sink.so" ]; then
    PA_MODULE_DIR="$dir"
    break
  fi
done
shopt -u nullglob
[ -n "$PA_ARCH_DIR" ] || { echo "PulseAudio root has no usr/lib/<architecture>/pulseaudio directory: $PA_ROOT" >&2; exit 1; }
[ -n "$PA_MODULE_DIR" ] || { echo "PulseAudio root lacks required modules under usr/lib/pulse-*/modules: $PA_ROOT" >&2; exit 1; }

PA_ARCH="$(basename "$(dirname "$PA_ARCH_DIR")")"
PA_LIB_DST="$STAGE/pulseaudio/usr/lib/$PA_ARCH"
PA_ARCH_DST="$PA_LIB_DST/pulseaudio"
if [ "$PA_ROOT" = / ]; then
  PA_MODULE_REL="${PA_MODULE_DIR#/}"
else
  PA_MODULE_REL="${PA_MODULE_DIR#"$PA_ROOT"/}"
fi
PA_MODULE_DST="$STAGE/pulseaudio/$PA_MODULE_REL"
PA_BIN_DST="$STAGE/pulseaudio/usr/bin"
mkdir -p "$PA_BIN_DST" "$PA_LIB_DST" "$PA_ARCH_DST" "$PA_MODULE_DST"
cp -L "$PA_BIN" "$PA_BIN_DST/pulseaudio.bin"
# 私有声卡使用自己的 socket。限定 daemon 的会话总线地址，避免与宿主 PulseAudio 争用 org.PulseAudio1。
cat > "$PA_BIN_DST/pulseaudio" <<'SH'
#!/bin/sh
set -eu
DBUS_SESSION_BUS_ADDRESS="unix:path=$PULSE_RUNTIME_PATH/dbus" exec "${0%/*}/pulseaudio.bin" "$@"
SH
chmod +x "$PA_BIN_DST/pulseaudio"
cp -L "$PA_MODULE_DIR/module-native-protocol-unix.so" "$PA_MODULE_DST/"
cp -L "$PA_MODULE_DIR/module-null-sink.so" "$PA_MODULE_DST/"

PA_LD_PATH="$(dirname "$PA_ARCH_DIR"):$PA_ARCH_DIR:$PA_MODULE_DIR"
copy_host_notice "$PA_BIN"
copy_host_notice "$PA_MODULE_DIR/module-native-protocol-unix.so"
copy_host_notice "$PA_MODULE_DIR/module-null-sink.so"
copy_elf_closure "$PA_BIN" "$PA_LIB_DST" "$PA_LD_PATH"
copy_elf_closure "$PA_MODULE_DIR/module-native-protocol-unix.so" "$PA_LIB_DST" "$PA_LD_PATH"
copy_elf_closure "$PA_MODULE_DIR/module-null-sink.so" "$PA_LIB_DST" "$PA_LD_PATH"

if [ "$PA_ROOT" != / ] && [ -d "$PA_ROOT/usr/share/doc" ]; then
  while IFS= read -r notice; do
    package="$(basename "$(dirname "$notice")")"
    cp -n "$notice" "$STAGE/THIRD_PARTY_LICENSES/$package-copyright" 2>/dev/null || true
  done < <(find "$PA_ROOT/usr/share/doc" -mindepth 2 -maxdepth 2 -type f -name copyright)
fi

{
  echo "name:    rig-edge-browser"
  echo "version: $VERSION"
  echo "git:     $GIT"
  echo "built:   $(date -u +%Y-%m-%dT%H:%M:%SZ)"
  echo "rustc:   $(rustc --version)"
  echo "host:    $(uname -srm)"
  echo "libc:    $(getconf GNU_LIBC_VERSION)"
  echo
  echo "# Bundled application libraries"
  find "$STAGE/lib" -maxdepth 1 -type f -printf '%f\n' | sort
  echo
  echo "# Bundled PulseAudio libraries"
  find "$PA_LIB_DST" -maxdepth 1 -type f -printf '%f\n' | sort
} > "$STAGE/BUILD-INFO.txt"

( cd "$SCRATCH" && tar czf "$ROOT/dist/$NAME.tar.gz" "$NAME" )
( cd "$ROOT/dist" && sha256sum "$NAME.tar.gz" > "$NAME.tar.gz.sha256" )
echo "Release package: dist/$NAME.tar.gz ($(du -h --apparent-size "$ROOT/dist/$NAME.tar.gz" | cut -f1))"
cat "$ROOT/dist/$NAME.tar.gz.sha256"
