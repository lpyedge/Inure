# eval/bench：难题页评测台（RIG 对 agent-browser）

用真实模型当操作者，在四个难题页上比较 **RIG**（`rig-edge-browser` 的 HTTP/批量动作接口）和
**agent-browser** 的调用数、被拒数、文本量与耗时，并用服务端事件判成败。方法与已测数据见
[docs/09 §6.2](../../docs/09-测试与评估.md)，决策规则与历次结果见 [decision.md](decision.md)。

| 题 | 页面 | 难点 |
|---|---|---|
| h1 | `site/index.html` | 80 行订单表、弹窗确认、同名复选框、iframe 设置面板、备注里的提示词注入 |
| h2 | `site/wizard.html` | 多步向导：cookie 横幅、校验、联动下拉、弹窗、同名干扰、诱导文字 |
| h3 | `site/expenses.html` | 45 行分页表、过滤、自动补全下拉、编辑弹窗、跨页选中、二次确认、懒加载标签页 |
| h4 | `site/support.html` + `ticket_frame.html` | iframe、shadow DOM 单选、自定义日历、长历史里找金额、两个同名 Send、视口外的保存 |

判定只看服务端记录的页面事件和按局随机的确认码；任何禁止动作（Cancel all、Delete account、
Approve all、Close all 等）一票否决。**评测台不含模型凭证；操作者用什么模型由你自己定**
（本仓库的数据用的是 `opencode-go/deepseek-v4.1-flash`，见 `run_batch.py`）。

## 依赖

- Python 3.10+，只用标准库（CDP 用 `cdp.py` 里的最小 WebSocket 客户端）。
- 已编译的 `rig-edge-browser`（默认找 `${CARGO_TARGET_DIR:-.temp/target}/debug/rig-edge-browser`，也可 `--bin`）。
- 对照臂需要 [agent-browser](https://github.com/vercel-labs/agent-browser) 命令行（`op_ab.py` 包装）。
- 并发跑模型操作者需要 `opencode` 命令行（`run_batch.py`）；不用它也可以自己拿 `prompts/<EP>.txt` 喂任意模型。

运行产物（`eps/ events/ logs/ prompts/ expected/ profiles/ site/data/`）都生成在本目录下并被 git 忽略。

## 跑一批

局名约定 `<批次>_<臂>_<题>_s<种子>`（`report.py` 靠它分组），每局必须是**全新的名字**。

```bash
cd eval/bench
cargo build -p rig-edge-browser                      # 仓库根目录
python3 start_server.py                              # 站点与事件服务，端口 18295
python3 launch_arm.py --bin <二进制> --port-base 18700 --n 3 --tag t --allow-origin http://127.0.0.1:18295
python3 expense_task.py 1 site expected              # 生成 h3 种子 1 的数据（h4 用 support_task.py；h1/h2 不需要）
python3 setup_a.py t newdoc4 h3 1 18700 e1_fix_h3_s1 # 重置页面并写出 prompts/e1_fix_h3_s1.txt（RIG 臂）
python3 setup_ab.py abs1 h3 1 e1_ab_h3_s1            # 同题的 agent-browser 臂
python3 run_batch.py --jobs 6 e1_fix_h3_s1 e1_ab_h3_s1
python3 report.py --prefix e1_                       # 判分并汇总
python3 teardown_arm.py t; bash stop_server.sh       # 收尾，并核查端口与残留进程
```

说明条件 `newdoc4`（批量、`replace`、`set_checked`、`observe_after`、`scroll_into_view`）取自产品文档；
**不要在说明里额外写解题提示**（上一轮的教训：没写批量用法，RIG 臂就吃亏）。其余可选条件见 `hard_prompt.py`。

`smoke_new.py h3|h4 <port> <seed> <EP>` 与 `smoke_hard.py` 是不调用模型的脚本化通关，用来确认页面、
判分和包装器本身是好的（先于任何模型评测跑一遍）。

## 进程纪律

测试台会拉起 Chromium、`rig-edge-browser`、`http.server` 和 agent-browser 会话。用完必须 `teardown_arm.py`、
`ab_teardown.py`、`stop_server.sh` 按 PID 精确清理并核查；**不要用 `pkill -f`**（模式串会匹配到自己的命令行）。
每批操作者控制在 12 个以内。
