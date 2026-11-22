# Edge Browser — Chromium 控制与 AI/人双通道

一个 Rust 二进制 `rig-edge-browser`，直连 Chromium（CDP），把**同一个真实浏览器**同时开放给：

- **AI 通道**：语义快照（无障碍树）+ 「帧号 + 元素 id」动作，执行前校验；
- **人通道**：画面与声音流（WebRTC / HLS / DOM 镜像）+ 鼠标键盘剪贴板输入。

人能看到 AI 在做什么，并随时强制接管。同机（stdio / 回环 HTTP）或局域网（HTTPS + 令牌）直连即完整形态。

## 快速开始

发布矩阵配置了五个原生目标：`linux-x86_64`、`linux-arm64`、`windows-x64`、`windows-arm64`、`macos-arm64`。每个目标生成 `full` 和 `slim` ZIP。Full 自带媒体运行库，Linux 还带 PulseAudio。Slim 解压后先部署运行库，再启动服务。Windows 对应的入口文件为 `rig-edge-browser.exe`。

```bash
./rig-edge-browser --install-deps
./rig-edge-browser --install-deps --with-chromium
./rig-edge-browser --install-deps --deps-base-url https://downloads.example.com/release
./rig-edge-browser --stdio
```

`--with-chromium` 会一并下载便携浏览器。依赖部署结束后命令退出。普通启动照常运行服务，浏览器按配置访问网站。Chromium 按包内 `chromium/`、系统安装、`--chromium-dir` 的顺序查找。

[依赖与实测环境](docs/04-依赖与环境.md) · [部署与运行](docs/05-编译部署与运行.md)

从源码开发（构建依赖与源码运行要求见 [docs/04](docs/04-依赖与环境.md)）：

```bash
# 编译（依赖见 docs/04-依赖与环境.md）
cargo build --release

# 人在本机看：打开 http://127.0.0.1:39871/ 并输入令牌
TOKEN=$(head -c 16 /dev/urandom | od -An -tx1 | tr -d ' \n')
RIG_EDGE_TOKEN="$TOKEN" target/release/rig-edge-browser --listen 127.0.0.1:39871 --human --url https://example.com

# AI 本机子进程（MCP stdio）：由智能体宿主拉起
target/release/rig-edge-browser --stdio --url https://example.com
```

## 文档

**从 [docs/README.md](docs/README.md) 开始**（10 篇文档集）：

| | |
|---|---|
| [01 产品与业务功能](docs/01-产品与业务功能.md) | 是什么、能做什么、不做什么 |
| [02 架构与技术方案](docs/02-架构与技术方案.md) | 模块、数据流、技术选型 |
| [03 关键技术难点与解决](docs/03-关键技术难点与解决.md) | 踩过的坑与方案 |
| [04 依赖与环境](docs/04-依赖与环境.md) | 依赖、全局安装、便携放置 |
| [05 编译、部署与运行](docs/05-编译部署与运行.md) | 编译、参数、运行配方、部署、运维 |
| [06 接口与调用](docs/06-接口与调用.md) | HTTP / MCP / 观看页 / 内嵌 |
| [07 智能体接入方案](docs/07-智能体接入方案.md) | Hermes、OpenClaw 等（预设计） |
| [08 安全模型](docs/08-安全模型.md) | 信任边界、令牌、威胁与对策 |
| [09 测试与评估](docs/09-测试与评估.md) | 测试分层、集成测试、度量规范 |
| [10 路线图与已知局限](docs/10-路线图与已知局限.md) | 需求、差距、局限 |

开发约束见 [CLAUDE.md](CLAUDE.md) 与 [AGENTS.md](AGENTS.md)。

## 常用脚本

| 脚本 | 用途 |
|---|---|
| `scripts/build.sh [debug\|release]` | 编译并检查动态库依赖 |
| `scripts/test.sh [unit\|it\|av\|py\|all]` | 测试入口（`it` 为真 Chromium 集成测试） |
| `scripts/local-gate.sh` | 提交前完整门禁 |
| `scripts/prepare-media.sh` | 从 Ubuntu 官方 APT 仓库下载 FFmpeg 和 PulseAudio 的 `.deb` 到 `.temp/media-deps/`，并解包供发布构建使用 |
| `scripts/release.sh` | 原生构建 full/slim ZIP、SHA-256 文件与依赖索引到 `dist/` |
| `scripts/docker-release.sh <release-prefix>` | 从 Linux ZIP 构建 Debian 13 Linux Docker 镜像（不推送） |
| `scripts/run.sh` | 本地快速运行（随机令牌 + 观看页） |
| `scripts/clean-tmp.sh` | 清理 `/tmp` 残余临时目录 |

详见 [docs/05 §1.3](docs/05-编译部署与运行.md)。

## 开发

```bash
source scripts/local-env.sh          # 构建缓存重定向到 .temp/（可选）
cargo fmt --all --check
cargo clippy --all-targets -- -D warnings
scripts/test.sh unit
scripts/test.sh it                   # 真 Chromium 集成测试（见 docs/09）
python3 -m unittest discover -s tests/eval_report -p 'test_*.py' -v
```

## 目录

| 路径 | 内容 |
|---|---|
| `crates/rig-edge-browser` | 边缘程序：Chromium 拉起/挂靠（CDP）、AI 通道（HTTP/MCP）、人通道（观看页、RTC/HLS）、鉴权 |
| `crates/rig-launcher` | 发布入口，负责依赖部署与启动边缘程序 |
| `crates/rig-proto` | AI 通道契约：`Observation`/`Action` 等共享类型 |
| `crates/rig-eval` | 感知层评估框架 |
| `eval/` | 评估任务、测试页面、历史运行证据（`results/` 只读） |
| `docs/` | 文档集（入口 `docs/README.md`） |
| `scripts/` | 构建/测试/发布/运行/清理脚本（见上表）、`edge-lan.sh`、`local-env.sh`、`eval_report.py` |
| `.temp/` | 本地可再生产物：构建缓存、日志、官方媒体软件包与解包文件 |

> 并发说明：按**单人受信工具**设计——多观看端并发时全局状态不保证相互独立。
> 网关/传输层已整仓移除（另有备份），本仓库只保留 Chromium 控制与 AI/人双通道。
