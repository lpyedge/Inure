#!/usr/bin/env bash
# 下载并解包 Ubuntu 媒体构建与运行依赖，保留原始软件包。
set -euo pipefail

cd "$(dirname "$0")/.."
ROOT="$PWD"
if [ "$(uname -s)" != Linux ]; then
  echo "Media dependency preparation requires native Ubuntu." >&2
  exit 1
fi
ARCH="$(dpkg --print-architecture)"
case "$ARCH" in
  amd64) TRIPLET=x86_64-linux-gnu; ARCHIVE=https://archive.ubuntu.com/ubuntu; SECURITY=https://security.ubuntu.com/ubuntu ;;
  arm64) TRIPLET=aarch64-linux-gnu; ARCHIVE=https://ports.ubuntu.com/ubuntu-ports; SECURITY="$ARCHIVE" ;;
  *) echo "Ubuntu media dependency preparation supports amd64 and arm64." >&2; exit 1 ;;
esac
DEST="$(realpath -m "${RIG_MEDIA_DIR:-$ROOT/.temp/media-deps}")"
EXPECTED_PC="$DEST/root/usr/lib/$TRIPLET/pkgconfig/libavcodec.pc"
EXPECTED_PA="$DEST/root/usr/bin/pulseaudio"

if [ ! -r /etc/os-release ] || [ ! -r /usr/share/keyrings/ubuntu-archive-keyring.gpg ]; then
  echo "Ubuntu release metadata or archive keyring is unavailable." >&2
  exit 1
fi
# shellcheck disable=SC1091
. /etc/os-release
if [ "${ID:-}" != ubuntu ] || [ -z "${VERSION_CODENAME:-}" ]; then
  echo "Media dependency preparation requires Ubuntu with VERSION_CODENAME." >&2
  exit 1
fi

if [ -e "$DEST" ]; then
  if [ -f "$EXPECTED_PC" ] && [ -x "$EXPECTED_PA" ] &&
     [ -f "$DEST/platform.txt" ] &&
     grep -Fxq "$VERSION_CODENAME:$ARCH" "$DEST/platform.txt"; then
    echo "Using prepared media dependencies: $DEST"
    exit 0
  fi
  echo "Media directory is incomplete or belongs to another Ubuntu release: $DEST" >&2
  echo "Move it aside before preparing media dependencies again." >&2
  exit 1
fi

for command in apt-get dpkg-deb; do
  if ! command -v "$command" >/dev/null 2>&1; then
    echo "Required command not found: $command" >&2
    exit 1
  fi
done

mkdir -p "$(dirname "$DEST")"
WORK="$(mktemp -d "$(dirname "$DEST")/media-prep.XXXXXX")"
STAGE="$WORK/prepared"
APT="$WORK/apt"
mkdir -p "$STAGE/debs" "$STAGE/root" "$APT/etc/parts" "$APT/etc/sourceparts" \
  "$APT/state/lists/partial" "$APT/cache/archives/partial" "$APT/log"
# 允许 APT 的下载进程访问本次准备的目录。
chmod 755 "$WORK"
trap 'rm -rf "$WORK"' EXIT

cat > "$APT/sources.list" <<EOF
deb [arch=$ARCH signed-by=/usr/share/keyrings/ubuntu-archive-keyring.gpg] $ARCHIVE $VERSION_CODENAME main universe
deb [arch=$ARCH signed-by=/usr/share/keyrings/ubuntu-archive-keyring.gpg] $ARCHIVE $VERSION_CODENAME-updates main universe
deb [arch=$ARCH signed-by=/usr/share/keyrings/ubuntu-archive-keyring.gpg] $SECURITY $VERSION_CODENAME-security main universe
EOF
: > "$APT/state/status"
cat > "$APT/apt.conf" <<EOF
Dir::Etc::parts "$APT/etc/parts";
Dir::Etc::main "$APT/etc/apt.conf";
Dir::Etc::sourcelist "$APT/sources.list";
Dir::Etc::sourceparts "$APT/etc/sourceparts";
Dir::State "$APT/state";
Dir::State::lists "$APT/state/lists";
Dir::State::status "$APT/state/status";
Dir::Cache "$APT/cache";
Dir::Cache::archives "$APT/cache/archives";
Dir::Cache::pkgcache "$APT/cache/pkgcache.bin";
Dir::Cache::srcpkgcache "$APT/cache/srcpkgcache.bin";
Dir::Log "$APT/log";
Acquire::Languages "none";
APT::Update::Error-Mode "any";
EOF

export APT_CONFIG="$APT/apt.conf"
apt-get update
apt-get --download-only --no-install-recommends -y install \
  libavcodec-dev libavformat-dev libavutil-dev libavfilter-dev libavdevice-dev \
  libswscale-dev libswresample-dev pulseaudio

find "$APT/cache/archives" -maxdepth 1 -type f -name '*.deb' -exec mv -t "$STAGE/debs" -- {} +
shopt -s nullglob
DEBS=("$STAGE/debs"/*.deb)
shopt -u nullglob
if [ "${#DEBS[@]}" -eq 0 ]; then
  echo "APT downloaded no media dependency packages." >&2
  exit 1
fi

for deb in "${DEBS[@]}"; do
  package="$(dpkg-deb -f "$deb" Source 2>/dev/null || true)"
  if [ -z "$package" ]; then package="$(dpkg-deb -f "$deb" Package)"; fi
  case "$package" in
    glibc|glibc\ *) continue ;;
  esac
  dpkg-deb -x "$deb" "$STAGE/root"
done

if [ ! -f "$STAGE/root/usr/lib/$TRIPLET/pkgconfig/libavcodec.pc" ] || \
   [ ! -x "$STAGE/root/usr/bin/pulseaudio" ]; then
  echo "Prepared dependency root is missing libavcodec.pc or pulseaudio." >&2
  exit 1
fi

printf '%s:%s\n' "$VERSION_CODENAME" "$ARCH" > "$STAGE/platform.txt"
mv -T "$STAGE" "$DEST"
echo "Prepared media dependencies: $DEST"
