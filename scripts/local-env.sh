#!/usr/bin/env bash
# 本地开发环境变量：把可再生缓存重定向到 .temp/（git 忽略），保持仓库干净。
# 仅本地生效，不影响他人；CI 对应配置见 .github/workflows/ci.yml。
#
# 用法（每次开新 shell 执行一次）：
#   source scripts/local-env.sh
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$ROOT/.temp/target}"
export PYTHONPYCACHEPREFIX="${PYTHONPYCACHEPREFIX:-$ROOT/.temp/pycache}"
