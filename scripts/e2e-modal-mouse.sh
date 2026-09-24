#!/bin/bash
# 真机端到端：弹窗按钮的**鼠标点击**能走完整条链路。
#
# 链路全真：TUI → daemon → provider（脚本化）→ ask 挂起 → 注入 SGR 鼠标事件 →
# 命中弹窗按钮 → 复用键盘那条应答路径 → 答案回到 provider → 回合继续。
#
# 为什么用注入而不是"手点"：鼠标事件就是终端发给进程的一串字节
# （`ESC[<0;列;行M` 按下 / `m` 松开 / `35` 移动），PTY 里可以直接写进去，
# 于是这条验收可以在 CI 里跑，不依赖有人真的拖鼠标。
#
# 判据（缺一即缺证）：
#   ① provider 收到了**被点击的那个选项**（不是默认第一项）——证明点击真的
#      命中了那一行，而不是碰巧提交了别的；
#   ② 回合在点击之后继续到最终正文（证明应答真的送达 daemon 并解除了挂起）；
#   ③ 结束时弹窗已消失。
#
# 用法：scripts/e2e-modal-mouse.sh
# 前置：两仓均已 `cargo build`。

set -u
SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
REPO_ROOT=$(cd -- "$SCRIPT_DIR/.." && pwd)
BACKEND_ROOT=${QAQH_BACKEND_ROOT:-$REPO_ROOT/../qaqh-backend-anchor}
DAEMON=${DAEMON:-$BACKEND_ROOT/target/debug/qaqh-daemon}
TUI=${TUI:-$REPO_ROOT/target/debug/qaqh-tui}
D=${D:-/tmp/qaqh-e2e-modal-mouse}
RUN_SECS=${RUN_SECS:-30}

case "$D" in
  /tmp/*|/var/tmp/*) ;;
  *) echo "拒绝：D 必须落在 /tmp 或 /var/tmp 下（当前：$D）——本脚本会对它 rm -rf" >&2; exit 1 ;;
esac

for b in "$DAEMON" "$TUI"; do
  [ -x "$b" ] || { echo "缺少可执行文件：$b"; echo "（本脚本不构建，请先 cargo build）"; exit 1; }
done
command -v python3 >/dev/null 2>&1 || { echo "需要 python3"; exit 1; }

rm -rf "$D"; mkdir -p "$D/qaqh" "$D/work"
DATA=$D/qaqh
WORK=$D/work

echo "跑弹窗鼠标点击回合（隔离 data root + workspace）…"
DAEMON="$DAEMON" TUI="$TUI" REPO_ROOT="$REPO_ROOT" DATA="$DATA" WORK="$WORK" \
  RUN_SECS="$RUN_SECS" python3 - <<'PY'
import json
import os
import pathlib
import subprocess
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

D = pathlib.Path(os.environ["DATA"]).parent
DATA = pathlib.Path(os.environ["DATA"])
WORK = pathlib.Path(os.environ["WORK"])
REPO_ROOT = pathlib.Path(os.environ["REPO_ROOT"])
RUN_SECS = float(os.environ["RUN_SECS"])

OPTIONS = ["方案甲", "方案乙", "方案丙"]
CLICKED = OPTIONS[1]
FINAL_TEXT = "鼠标点击已生效：选项已回传"


class FakeProvider(BaseHTTPRequestHandler):
    def log_message(self, fmt, *args):
        pass

    def do_POST(self):
        if self.path not in ("/v1/chat/completions", "/chat/completions"):
            self.send_error(404)
            return
        length = int(self.headers.get("Content-Length", "0"))
        payload = json.loads(self.rfile.read(length) or b"{}")
        self.server.requests.append(payload)

        if not payload.get("stream"):
            body = json.dumps(
                {"choices": [{"message": {"role": "assistant", "content": "鼠标冒烟"}}]}
            ).encode()
            self.send_response(200)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)
            return

        turn = self.server.main_calls
        self.server.main_calls += 1

        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.send_header("Cache-Control", "no-cache")
        self.end_headers()

        if turn == 0:
            chunks = [
                {"choices": [{"index": 0, "delta": {"role": "assistant"}, "finish_reason": None}]},
                {
                    "choices": [
                        {
                            "index": 0,
                            "delta": {
                                "tool_calls": [
                                    {
                                        "index": 0,
                                        "id": "call_ask_mouse",
                                        "type": "function",
                                        "function": {
                                            "name": "ask",
                                            "arguments": json.dumps(
                                                {
                                                    "question": "选哪个方案继续？",
                                                    "options": OPTIONS,
                                                    "allow_custom": False,
                                                }
                                            ),
                                        },
                                    }
                                ]
                            },
                            "finish_reason": None,
                        }
                    ]
                },
                {"choices": [{"index": 0, "delta": {}, "finish_reason": "tool_calls"}]},
            ]
        else:
            chunks = [
                {
                    "choices": [
                        {
                            "index": 0,
                            "delta": {"role": "assistant", "content": FINAL_TEXT},
                            "finish_reason": None,
                        }
                    ]
                },
                {"choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}]},
            ]
        for chunk in chunks:
            self.wfile.write(f"data: {json.dumps(chunk)}\n\n".encode())
            self.wfile.flush()
            time.sleep(0.02)
        self.wfile.write(b"data: [DONE]\n\n")
        self.wfile.flush()


server = ThreadingHTTPServer(("127.0.0.1", 0), FakeProvider)
server.main_calls = 0
server.requests = []
port = server.server_address[1]
threading.Thread(target=server.serve_forever, daemon=True).start()
print(f"fake provider: http://127.0.0.1:{port}/v1")

(DATA / "config.toml").write_text(
    f'''provider_id = "openai"
active_profile = "default"
permission_level = 3

[profiles.default]
model = "fake-model"
max_tokens = 4096
effort = "low"
context_limit = 100000
base_url = "http://127.0.0.1:{port}/v1"
endpoint = "openai"
'''
)

daemon_env = os.environ.copy()
daemon_env["QAQH_DATA_DIR"] = str(DATA)
daemon = subprocess.Popen(
    [os.environ["DAEMON"], "run"],
    stdin=subprocess.DEVNULL,
    stdout=(D / "daemon.out").open("ab"),
    stderr=subprocess.STDOUT,
    env=daemon_env,
    start_new_session=True,
)
deadline = time.monotonic() + 20
while time.monotonic() < deadline:
    try:
        discovery = json.loads((DATA / "daemon.json").read_text())
    except (OSError, json.JSONDecodeError):
        time.sleep(0.2)
        continue
    if discovery.get("pid") == daemon.pid:
        break
else:
    raise SystemExit("daemon did not publish daemon.json")
print(f"daemon pid={daemon.pid}")

# ── 目标坐标 ─────────────────────────────────────────────────────────────
# 与 `ui/v2/modal.rs` 的布局**逐式对齐**（改布局这里会红，这是有意的 pin）：
#   ask_rect: 宽 = min(88, 列-4)，高 = min(行-4, 30)，在整屏居中
#   ask_rows: 问题(1 行) → 空行 → 选项 0 → 选项 1 → …
# pty-driver 默认 40 行 × 130 列。
ROWS, COLS = 40, 130
width = min(88, COLS - 4)
height = min(ROWS - 4, 30)
rect_x = (COLS - width) // 2
rect_y = (ROWS - height) // 2
inner_x, inner_y = rect_x + 1, rect_y + 1
option_index = 1
# 1 行问题 + 1 行空行 + 前面几个选项
row = inner_y + 2 + option_index
col = inner_x + 4
# SGR 鼠标是 1-based。
sgr_row, sgr_col = row + 1, col + 1


def sgr(button: int, final: str) -> str:
    return f"1b5b3c{button};{sgr_col};{sgr_row}{final}".replace("1b5b3c", "1b5b3c")


move = bytes(f"\x1b[<35;{sgr_col};{sgr_row}M", "ascii").hex()
press = bytes(f"\x1b[<0;{sgr_col};{sgr_row}M", "ascii").hex()
release = bytes(f"\x1b[<0;{sgr_col};{sgr_row}m", "ascii").hex()
print(f"目标：第 {sgr_row} 行 第 {sgr_col} 列（选项 {option_index + 1}: {CLICKED}）")

tui_env = os.environ.copy()
tui_env["QAQH_DATA_DIR"] = str(DATA)
driver = [
    "python3",
    str(REPO_ROOT / "scripts" / "lib" / "pty-driver.py"),
    "--tui", os.environ["TUI"],
    "--raw", str(D / "tui.raw"),
    "--seconds", str(RUN_SECS),
    "--key", "4:0e",                       # Ctrl+N
    "--type", "11:帮我选一个方案",
    "--key", "13:0d",                      # 回车发出
    "--key", f"19:{move}",                 # 悬停到目标选项
    "--key", f"20:{press}",                # 按下
    "--key", "20.4:" + release,            # 松开 → 提交
    "--quit",
]
try:
    subprocess.run(driver, cwd=WORK, env=tui_env, check=True, timeout=RUN_SECS + 30)
finally:
    try:
        daemon.terminate()
        daemon.wait(timeout=5)
    except Exception:
        daemon.kill()

(D / "provider-requests.json").write_text(
    json.dumps(server.requests, ensure_ascii=False, indent=2), encoding="utf-8"
)
print(f"provider 主回合调用次数: {server.main_calls}")
print(f"产物目录: {D}")
PY

echo
echo "== 判据 =="
python3 - "$D" "$REPO_ROOT" <<'PY'
import json
import pathlib
import sys

D = pathlib.Path(sys.argv[1])
REPO_ROOT = pathlib.Path(sys.argv[2])
CLICKED = "方案乙"
FINAL_TEXT = "鼠标点击已生效"

sys.path.insert(0, str(REPO_ROOT / "scripts" / "lib"))
from vt_screen import render_text

raw = (D / "tui.raw").read_text(errors="replace")
screen = render_text(raw)
requests = json.loads((D / "provider-requests.json").read_text(encoding="utf-8"))

# ① 答案真的回了 provider：ask 的工具结果里带着被点的那个选项。
answered = any(
    m.get("role") == "tool" and CLICKED in str(m.get("content"))
    for body in requests
    for m in (body.get("messages") or [])
)
# 反例闸：默认第一项**不该**出现——否则说明点击没命中、只是碰巧提交了默认值。
wrong_default = any(
    m.get("role") == "tool" and "方案甲" in str(m.get("content"))
    for body in requests
    for m in (body.get("messages") or [])
)

# ③ 弹窗确实关掉了。**不能用"重建屏幕里没有弹窗标题"判**：`render_text` 不处理
# alternate screen 切换，退出 alt screen 后旧内容还留在它的虚拟屏上（实测会假红）。
# 直接看字节流里的模式位：进 alt screen / 出 alt screen。
left_alternate = "\x1b[?1049l" in raw
# ④ 捕获纪律：鼠标捕获**只在弹窗期间**开（进弹窗开、出弹窗关）。这条是"不破坏
# 主界面原生选择/复制"的机械保证，比任何 UI 断言都硬。
capture_on = "\x1b[?1000h" in raw and "\x1b[?1003h" in raw and "\x1b[?1006h" in raw
capture_off = "\x1b[?1000l" in raw and "\x1b[?1003l" in raw and "\x1b[?1006l" in raw

# ⑤ 悬停/按下必须有**可见**反馈，但两种终端的可见形式不同，判据取"任一"：
#   - 有彩色：底色（`ESC[48;2;…m` 真彩色 / `ESC[48;5;…m` 256 色）；
#   - 无彩色（`NO_COLOR` / `terminal` 主题）：反显 `ESC[7m`。
# 只认一种会误判另一种（实测：NO_COLOR 下写死 `48;` 会假红，反之亦然）。
import re as _re
# ⚠ 底色不能锚在 `ESC[48;` 上：crossterm 会把前景与背景合并成一条
# `ESC[38;2;…;48;2;…m`（实测），锚死开头就永远匹配不到。
_RAW = (D / "tui.raw").read_bytes()
hover_cue = bool(
    _re.search(rb"48;[0-9;]*m", _RAW) or _re.search(rb"\x1b\[7m", _RAW)
)

checks = [
    ("① provider 收到被点击的选项（方案乙）", answered),
    ("⑤ 悬停/按下有可见反馈（反显序列）", hover_cue),
    ("① 反例：没有误提交默认第一项（方案甲）", not wrong_default),
    ("② 点击后回合继续到最终正文", FINAL_TEXT in screen),
    ("③ 弹窗关闭（离开 alternate screen）", left_alternate),
    ("④ 鼠标捕获只在弹窗期间开（开+关都在）", capture_on and capture_off),
]
ok = True
for name, hit in checks:
    ok &= hit
    print(f"  [{'✓' if hit else '✗'}] {name}")
if not ok:
    print("--- 重建后的屏幕 ---")
    print(screen[:2000])
print("RESULT:", "PASS" if ok else "FAIL")
sys.exit(0 if ok else 1)
PY
