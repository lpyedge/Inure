#!/usr/bin/env bash
# Build local Docker images from Linux release bundles; host labels map to Linux OCI platforms.
set -euo pipefail
cd "$(dirname "$0")/.."
ROOT="$PWD"

usage() {
  cat <<'HELP'
Usage: scripts/docker-release.sh <release-prefix> [host-label ...] [--variant full|slim]

The prefix is everything before the target and variant, including the
trailing hyphen, for example:
  scripts/docker-release.sh dist/rig-edge-browser-1.2.3-abcd-

Host labels (each builds a Linux workload; no native Windows/macOS execution):
  linux-x86_64  -> linux/amd64
  linux-arm64   -> linux/arm64
  windows-x64   -> linux/amd64
  windows-arm64 -> linux/arm64
  macos-arm64   -> linux/arm64

By default all five labels and both full/slim variants are built. Optionally
pass one or more labels and --variant full|slim. OCI builds are deduplicated by
actual platform and variant. This script uses buildx --load and never pushes.

Full images contain the bundle at /opt/rig. Slim images install media
dependencies when started with --install-deps.
For persistent slim dependencies, mount a writable named volume at /opt/rig
so the launcher can atomically swap runtime/ and chromium/:
  docker volume create rig-data
  docker run -v rig-data:/opt/rig IMAGE --install-deps
  TOKEN=$(head -c 16 /dev/urandom | od -An -tx1 | tr -d ' \n')
  docker run -v rig-data:/opt/rig -p 127.0.0.1:39871:39871 -e RIG_EDGE_TOKEN="$TOKEN" IMAGE

Debian 13 Chromium and CA certificates are already hydrated at image build time.
Use --install-deps when the selected release bundle supports runtime hydration.

HTTP mode requires RIG_EDGE_TOKEN (or an explicit --token argument).
HELP
}

die() { echo "Error: $*" >&2; exit 2; }
if [[ ${1:-} == --help || ${1:-} == -h ]]; then usage; exit 0; fi
[[ $# -ge 1 ]] || { usage >&2; exit 2; }
PREFIX="$1"; shift
[[ "$PREFIX" == *- ]] || die "release prefix must end with a hyphen"
[[ "$PREFIX" != *-full- && "$PREFIX" != *-slim- ]] || die "prefix must stop before architecture and flavor"

HOSTS=()
VARIANTS=(full slim)
while (($#)); do
  case "$1" in
    --help|-h) usage; exit 0 ;;
    --variant)
      (($# >= 2)) || die "--variant requires full or slim"
      case "$2" in full|slim) VARIANTS=("$2");; *) die "unknown variant: $2";; esac
      shift 2 ;;
    linux-x86_64|linux-arm64|windows-x64|windows-arm64|macos-arm64)
      HOSTS+=("$1"); shift ;;
    *) die "unknown argument: $1 (use --help)" ;;
  esac
done
if ((${#HOSTS[@]} == 0)); then
  HOSTS=(linux-x86_64 linux-arm64 windows-x64 windows-arm64 macos-arm64)
fi

command -v docker >/dev/null || die "docker is required"
docker buildx version >/dev/null 2>&1 || die "Docker buildx is required"
[[ -f scripts/release.Dockerfile ]] || die "missing scripts/release.Dockerfile"

TAG_PREFIX="${PREFIX##*/}"
TAG_PREFIX="${TAG_PREFIX#rig-edge-browser-}"
DONE=" "
for host in "${HOSTS[@]}"; do
  case "$host" in
    linux-x86_64|windows-x64) arch=linux-x86_64; platform=linux/amd64 ;;
    linux-arm64|windows-arm64|macos-arm64) arch=linux-arm64; platform=linux/arm64 ;;
  esac
  for variant in "${VARIANTS[@]}"; do
    key="$platform/$variant"
    image="rig-edge-browser:${TAG_PREFIX}${arch}-${variant}"
    if [[ "$DONE" != *" $key "* ]]; then
      bundle="${PREFIX}${arch}-${variant}.zip"
      [[ -f "$bundle" ]] || die "release bundle not found: $bundle"
      echo "Building $platform from $bundle as $image"
      docker buildx build --platform "$platform" --load \
        --build-arg "BUNDLE_FILE=$(basename "$bundle")" \
        -f "$ROOT/scripts/release.Dockerfile" -t "$image" "$(dirname "$bundle")"
      DONE="$DONE$key "
    fi
    alias="rig-edge-browser:${TAG_PREFIX}${host}-${variant}"
    if [[ "$image" != "$alias" ]]; then
      docker tag "$image" "$alias"
    fi
    echo "Mapped Linux workload: $host -> $platform ($alias)"
  done
done
