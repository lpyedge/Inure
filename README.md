# Edge Browser — Chromium 控制与 AI/人双通道

边缘程序通过 CDP 直连 Chromium，让人和 AI 共用同一个浏览器：

- **AI 通道**：语义快照（无障碍树）+ 「帧号 + 元素 id」动作，执行前校验；
- **人通道**：画面与声音流（WebRTC / HLS / DOM 镜像）+ 鼠标键盘剪贴板输入。

人能看到 AI 在做什么，并随时强制接管。同机（stdio / 回环 HTTP）或局域网（HTTPS + 令牌）直连即完整形态。

## 快速开始

发布矩阵配置了五个原生目标：`linux-x86_64`、`linux-arm64`、`windows-x64`、`windows-arm64`、`macos-arm64`。每个目标生成 `full` 和 `slim` ZIP。Full 自带媒体运行库，Linux 还带 PulseAudio。Slim 解压后先部署运行库，再启动服务。Windows 对应的入口文件为 `rig-edge-browser.exe`。

```bash
# Slim 包先安装媒体库。需要便携 Chromium 时加 --with-chromium。
./rig-edge-browser --install-deps
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

查看全部命令行参数：`rig-edge-browser --help`（参数详解见 [docs/05 §2](docs/05-编译部署与运行.md)）。

## 文档

项目说明与专题设计见 [文档地图](docs/README.md)。构建、测试、发布与运行脚本见 [部署与脚本](docs/05-编译部署与运行.md)，开发约束见 [CLAUDE.md](CLAUDE.md) 与 [AGENTS.md](AGENTS.md)。

## 开发

```bash
source scripts/local-env.sh  # 可选，将构建缓存重定向到 .temp/
scripts/test.sh unit         # Rust 测试，跳过真 Chromium 用例
scripts/local-gate.sh        # 文档检查、fmt、clippy、Rust（含真 Chromium）及 Python 自测
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
| `scripts/` | 构建、测试、发布、运行和清理脚本 |
| `.temp/` | 本地可再生产物：构建缓存、日志、官方媒体软件包与解包文件 |

> 并发说明：按**单人受信工具**设计——多观看端并发时全局状态不保证相互独立。
