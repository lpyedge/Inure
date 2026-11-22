# Edge Browser — Chromium 控制与 AI/人双通道

一个 Rust 二进制 `rig-edge-browser`，直连 Chromium（CDP），把**同一个真实浏览器**同时开放给：

- **AI 通道**：语义快照（无障碍树）+ 「帧号 + 元素 id」动作，执行前校验；
- **人通道**：画面与声音流（WebRTC / HLS / DOM 镜像）+ 鼠标键盘剪贴板输入。

人能看到 AI 在做什么，并随时强制接管。同机（stdio / 回环 HTTP）或局域网（HTTPS + 令牌）直连即完整形态。

## 快速开始

将发布包完整解压，执行其中的 `./rig-edge-browser --stdio`。包内已带 FFmpeg 共享库和 PulseAudio。系统 Chromium 从 `PATH` 查找，便携浏览器用 `--chromium-dir /path/to/chrome-linux64` 指定。观看页、局域网和持久化配置见 [运行配方](docs/05-编译部署与运行.md#3-运行配方)。当前验证环境为 Ubuntu 26.04、Linux x86_64、glibc 2.43。

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
| `scripts/release.sh` | 打包程序、FFmpeg 共享库和 PulseAudio 到 `dist/`（tar.gz + sha256）；发布/升级需使用完整归档 |
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
| `crates/rig-proto` | AI 通道契约：`Observation`/`Action` 等共享类型 |
| `crates/rig-eval` | 感知层评估框架 |
| `eval/` | 评估任务、测试页面、历史运行证据（`results/` 只读） |
| `docs/` | 文档集（入口 `docs/README.md`） |
| `scripts/` | 构建/测试/发布/运行/清理脚本（见上表）、`edge-lan.sh`、`local-env.sh`、`eval_report.py` |
| `.temp/` | 本地可再生产物：构建缓存、日志、便携 PulseAudio；运行临时目录在系统临时目录下（git 忽略） |

> 并发说明：按**单人受信工具**设计——多观看端并发时全局状态不保证相互独立。
> 网关/传输层已整仓移除（另有备份），本仓库只保留 Chromium 控制与 AI/人双通道。
