#!/usr/bin/env python3
"""hard_prompt.py <arm: rig|ab> <cond> <task: h1|h2> <EP> <port|session> [seed]

Writes the operator prompt to stdout. arm=rig: cond is base|new|routed (port = RIG port).
arm=ab: cond is ab (session = agent-browser session name). task h2 needs a seed (its task text is generated).
The page is already open when the operator starts; the runner resets it with op.py/op_ab.py reset --url.
"""
import os
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
import wizard_task  # noqa: E402

VENV = sys.executable
H1_TASK = ("在订单页给订单 ORD-0057（客户 Mara Lindqvist）发货（点 Ship，在弹出的对话框里确认），"
           "然后在页面下方 Settings 面板里把 Region 设为 West，勾选 Send updates，Note 填 ship-done，"
           "点 Save settings，最后用 answer 命令报告 Save 之后页面显示的确认码。不要做任何别的操作。")


DOC_BLOCK = """
动作文档（摘自产品文档）：
  type：`text`；可选 `target`（给了先点击并复核焦点）、`submit`（回车）。key：`key`，如 "Enter"、"Control+A"。select：原生 <select>。
  批量动作：把 `action` 换成 `actions`（1–8 个动作）：{"frame_id":2,"actions":[{…},{…}]}。按序执行，每一步都以同一个 frame_id 独立走完整校验，遇到第一个非 done 立即停止；回包里有每一步的结果。一次调用可以连续做多个动作（例如先选下拉再输入再点击），被拒时同样附最新状态。
"""


DOC_BLOCK3 = """
动作文档（摘自产品文档）：
  type：`text`；可选 `target`（给了先点击并复核焦点）、`submit`（回车）。key：`key`，如 "Enter"、"Control+A"。select：原生 <select>。
  act 一次接受 1–8 个动作（actions 列表）：按序执行，每一步都以同一个 frame_id 独立走完整校验，遇到第一个非 done 立即停止；回包里有每一步的结果。一次调用可以连续做多个动作（例如先选下拉再输入再点击），被拒时同样附最新状态。
"""


DOC_BLOCK4 = """  批量的 `observe_after`：批量请求可加 `"observe_after":"delta"`（或 `"text"`），整批全部 done 后回包会多一个 `text`：delta 是相对本次 frame_id 的文本行增量（'~ 行' 替换同 id 的行、'+ 行' 插入、'- [id]' 删除，'# frame N (unchanged …)' 表示没变），text 是完整文本；只回文本，不附截图。回包里的 `# frame N` 可直接作为下一次动作的 frame_id，不必再 observe。缺省不附。
"""


DOC_BLOCK5 = """  scroll_into_view：{"kind":"scroll_into_view","target":{"by":"element","id":"e9"}}，把一个已观察到的元素（包括视口外的、用 `observe --offscreen` 列出的元素，甚至在 iframe 里的）滚入视口；之后需要再 observe。`observe --offscreen` 额外列出视口外的元素（标注 Offscreen）。
"""


DOC_BLOCK2 = """  type 的 `replace`：缺省 false 追加；`"replace":true` 先全选再输入，覆盖已有内容（例如 {"kind":"type","target":{...},"text":"新内容","replace":true}）。
  set_checked：{"kind":"set_checked","target":{"by":"element","id":"e5"},"checked":true}，幂等地把复选框/单选框/开关设为已选(true)或未选(false)，已经是目标状态就不动，不像 click 那样来回切换。
"""


BATCH_EXAMPLES = """act 的 json 只有一种形状：{"frame_id":N,"actions":[动作1,动作2,…]}，actions 是 1–8 个动作（frame_id 用你最近一次 observe 回包里的帧号）。只做一个动作时也写成只有一个元素的列表。例子：
  点元素：{"frame_id":9,"actions":[{"kind":"click","target":{"by":"element","id":"e9"}}]}
  点坐标（页面逻辑像素，左上角为原点）：{"frame_id":7,"actions":[{"kind":"click","target":{"by":"point","x":153,"y":303}}]}
  下拉选择：{"frame_id":13,"actions":[{"kind":"select","target":{"by":"element","id":"e357"},"values":["Sand"]}]}
  输入文字：{"frame_id":14,"actions":[{"kind":"type","target":{"by":"element","id":"e366"},"text":"4165"}]}
  按键：{"frame_id":14,"actions":[{"kind":"key","key":"Enter"}]}（组合键如 "Control+Backspace"，可按词删除）
  滚动页面：{"frame_id":14,"actions":[{"kind":"scroll","dx":0,"dy":700}]}（observe 只返回视口内的元素，视口外的内容要先滚动）
  一次做多步：{"frame_id":14,"actions":[{"kind":"select","target":{"by":"element","id":"e4"},"values":["Sand"]},{"kind":"type","target":{"by":"element","id":"e5"},"text":"4165"},{"kind":"click","target":{"by":"element","id":"e6"}}]}
"""


SINGLE_EXAMPLES = """act 的 json 例子（frame_id 用你最近一次 observe 回包里的帧号）：
  点元素：{"frame_id":9,"action":{"kind":"click","target":{"by":"element","id":"e9"}}}
  点坐标（页面逻辑像素，左上角为原点）：{"frame_id":7,"action":{"kind":"click","target":{"by":"point","x":153,"y":303}}}
  下拉选择：{"frame_id":13,"action":{"kind":"select","target":{"by":"element","id":"e357"},"values":["Sand"]}}
  输入文字：{"frame_id":14,"action":{"kind":"type","target":{"by":"element","id":"e366"},"text":"4165"}}
  按键：{"frame_id":14,"action":{"kind":"key","key":"Enter"}}（组合键如 "Control+Backspace"，可按词删除）
  滚动页面：{"frame_id":14,"action":{"kind":"scroll","dx":0,"dy":700}}（observe 只返回视口内的元素，视口外的内容要先滚动）
"""


def rig_doc(cond, port, ep, task):
    docs = cond in ("newdoc", "newdoc2", "newdoc3", "newdoc4")
    docs2 = cond in ("newdoc2", "newdoc3", "newdoc4")
    docs3 = cond == "newdoc3"
    cond4 = cond == "newdoc4"
    if docs:
        cond = "new"
    base = f"cd {HERE} && COND={cond} EP={ep}{' BATCH_ONLY=1' if docs3 else ''} {VENV} op.py {port}"
    extra = ""
    if cond == "routed":
        extra = (f"  {base} shot --clip x,y,w,h     只截取视口里这块区域（页面逻辑像素，原生分辨率）；会打印实际区域 "
                 "x-rig-clip 和 x-rig-image-scale。图里一点 (px,py) 对应页面坐标 (x+px/scale, y+py/scale)；动作里的坐标仍然用整页坐标。\n"
                 f"  {base} observe --mode text --layout blocks   更紧凑的文本排版（表格行等，不含坐标）\n")
    return f"""你是网页操作助手，用命令行工具操作一个已经打开的网页，完成下面的任务。页面已经打开，直接开始，不要重新打开。

任务：{task}

可用命令（只能用这几个，每次都要完整写出前缀）：
  {base} observe [--mode text|tree]   观察页面，回包里有“# frame N”帧号和元素 id（如 e12）
  {base} act '<json>'                 执行一个动作
  {base} shot                         截图，PNG 保存到文件，路径会打印出来，请用 Read 工具查看图片
{extra}  {base} answer <确认码>            报告确认码（只在任务要求报告时用，参数就是码本身）
{BATCH_EXAMPLES if docs3 else SINGLE_EXAMPLES}
动作被拒绝时不会执行，请根据回包提示自己决定下一步。
{((DOC_BLOCK3 if docs3 else DOC_BLOCK) + (DOC_BLOCK2 if docs2 else "") + (DOC_BLOCK4 if cond4 else "") + (DOC_BLOCK5 if cond4 else "")) if docs else ""}
规则：最多 80 次命令调用；不要读 {HERE} 下的任何文件（除你自己截图以外），不要用 curl 或其他方式直连页面；
任务完成就停下，最后回复一行 DONE 加一句话说明做了什么。"""


def ab_doc(session, ep, task):
    base = f"cd {HERE} && EP={ep} {VENV} op_ab.py {session}"
    return f"""你是网页操作助手，用命令行工具操作一个已经打开的网页，完成下面的任务。页面已经打开，直接开始，不要重新打开。

任务：{task}

可用命令（只能用这几个，每次都要完整写出前缀）：
  {base} snapshot             无障碍树，元素带 ref（如 @e1），之后可用 @ref 指代元素；页面变化后需重新 snapshot
  {base} screenshot           截图，PNG 保存到文件，路径会打印出来，请用 Read 工具查看图片（视口 1280x800，未缩放）
  {base} scrollintoview @ref   把元素滚入视口（视口外元素要先滚入再点击）
  {base} scroll down 700      滚动页面
  {base} click @ref           点击元素，例如 click @e9
  {base} fill @ref 文本        清空并填入文字，例如 fill @e5 4165
  {base} select @ref 值        下拉选择，例如 select @e4 Sand
  {base} check @ref / uncheck @ref   勾选 / 取消勾选复选框
  {base} press 键               按键，例如 press Enter
  {base} mouse move x y        把鼠标移到视口像素坐标 (x,y)；mouse down / mouse up 为左键按下与抬起
  {base} answer <确认码>        报告确认码（只在任务要求报告时用，参数就是码本身）

规则：最多 80 次命令调用；不要读 {HERE} 下的任何文件（除你自己截图以外），不要用 curl 或其他方式直连页面，
不要使用上面列表之外的 agent-browser 子命令；任务完成就停下，最后回复一行 DONE 加一句话说明做了什么。"""


def main():
    if len(sys.argv) not in (6, 7):
        raise SystemExit(__doc__.strip().splitlines()[0] + "\nusage: hard_prompt.py <rig|ab> <cond> <h1|h2> <EP> <port|session> [seed]")
    arm, cond, task_name, ep, target = sys.argv[1:6]
    seed = sys.argv[6] if len(sys.argv) == 7 else None
    if task_name == "h1":
        task = H1_TASK
    elif task_name in ("h3", "h4"):
        if seed is None or not seed.isdigit():
            raise SystemExit(f"{task_name} needs an integer seed")
        mod = __import__("expense_task" if task_name == "h3" else "support_task")
        task = mod.get_task(seed)[0]
    elif task_name == "h2":
        if seed is None or not seed.isdigit():
            raise SystemExit("h2 needs an integer seed")
        task, _ = wizard_task.get_task(seed)
    else:
        raise SystemExit("task must be h1 or h2")
    if arm == "rig":
        if cond not in ("base", "new", "routed", "newdoc", "newdoc2", "newdoc3", "newdoc4") or not target.isdigit():
            raise SystemExit("rig: cond base|new|routed and a numeric port")
        print(rig_doc(cond, int(target), ep, task))
    elif arm == "ab":
        if cond != "ab":
            raise SystemExit("ab: cond must be ab")
        print(ab_doc(target, ep, task))
    else:
        raise SystemExit("arm must be rig or ab")


if __name__ == "__main__":
    main()
