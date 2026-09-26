#!/bin/bash
# 真机端到端：HitMap 的 **PTY 事件矩阵**（SGR 按下/抬起/移动/滚轮 + 跨 chunk +
# 合批 + route/resize 切换），全程在 `QAQH_HIT_PROBE=strict` 下跑。
#
# 出处：`docs/spec/2026-09-26-v2指针按钮与命中测试-spec.md` §9.4 / §10，
#       `docs/plan/2026-09-26-v2指针按钮与命中测试-plan.md` P0-C-7。
#
# 为什么需要它：单测能锁住"给定 HitMap，dispatch 会怎么走"；但**只有真 PTY**
# 能证明三件事：
#   ① crossterm 把跨 chunk 切断的 SGR 序列拼回一个事件，命中仍然对；
#   ② 滚轮 / resize 之后 TUI 仍然重排、重画、重新发布帧，且 strict 探针
#      在真实 buffer 上零失败（探针误报会让 TUI 直接带诊断退出）；
#   ③ 鼠标捕获进入/退出成对。
#
# 用法：scripts/e2e-hitmap-pty.sh
# 前置：两仓均已 `cargo build`（脚本不构建）。
# 隔离：私有 QAQH_DATA_DIR（/tmp 下）+ 私有 fake provider，不碰在用的 daemon。

set -u
SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
REPO_ROOT=$(cd -- "$SCRIPT_DIR/.." && pwd)
BACKEND_ROOT=${QAQH_BACKEND_ROOT:-$REPO_ROOT/../qaqh-backend}
DAEMON=${DAEMON:-$BACKEND_ROOT/target/debug/qaqh-daemon}
TUI=${TUI:-$REPO_ROOT/target/debug/qaqh-tui}
D=${D:-/tmp/qaqh-e2e-hitmap-pty}
RUN_SECS=${RUN_SECS:-36}
TUI_ARGS=${TUI_ARGS:-}

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

echo "跑 HitMap PTY 事件矩阵（隔离 data root + workspace，QAQH_HIT_PROBE=strict）…"
DAEMON="$DAEMON" TUI="$TUI" REPO_ROOT="$REPO_ROOT" DATA="$DATA" WORK="$WORK" \
  RUN_SECS="$RUN_SECS" TUI_ARGS="$TUI_ARGS" python3 - <<'PY'
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
FINAL_MARKER = "命中矩阵正文"
# 足够长，保证 transcript 超出一屏 → 滚轮之后「回到最新消息」按钮才可能出现。
FINAL_TEXT = FINAL_MARKER + "\n\n" + "\n".join(
    f"- 第 {i} 行：鼠标命中链路在 strict 探针下保持自洽" for i in range(1, 61)
)


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
                {"choices": [{"message": {"role": "assistant", "content": "命中矩阵冒烟"}}]}
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
                                        "id": "call_ask_hitmap",
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
# 与 `ui/v2/modal.rs` 的布局逐式对齐（pty-driver 默认 40 行 × 130 列）：
#   ask_rect: 宽 = min(88, 列-4)，高 = min(行-4, 30)，整屏居中
#   ask_rows: 问题(1 行) → 空行 → 选项 0 → 选项 1 → …
ROWS, COLS = 40, 130
width = min(88, COLS - 4)
height = min(ROWS - 4, 30)
rect_x = (COLS - width) // 2
rect_y = (ROWS - height) // 2
inner_x, inner_y = rect_x + 1, rect_y + 1
option_index = 1
row = inner_y + 2 + option_index
col = inner_x + 4
# SGR 鼠标是 1-based。
sgr_row, sgr_col = row + 1, col + 1
print(f"目标：第 {sgr_row} 行 第 {sgr_col} 列（选项 {option_index + 1}: {CLICKED}）")


def hexed(payload: bytes) -> str:
    return payload.hex()


def sgr_at(button: int, final: str, col: int, row: int) -> bytes:
    """SGR 鼠标序列；col/row 是 **1-based** 终端坐标。"""
    return f"\x1b[<{button};{col};{row}{final}".encode("ascii")


move = sgr_at(35, "M", sgr_col, sgr_row)
press = sgr_at(0, "M", sgr_col, sgr_row)
release = sgr_at(0, "m", sgr_col, sgr_row)
# 滚轮（SGR button 64 = 向上滚）不依赖坐标，但坐标仍要给合法值。
wheel_up = b"\x1b[<64;10;10M"

# 正文区里任意一行都是消息行（assistant 块远长于一屏，可见窗口整段落在它里面）。
# y=20 在 40 行终端里一定落在 body 内。
MSG_COL, MSG_ROW = 5, 20
msg_click = sgr_at(0, "M", MSG_COL + 1, MSG_ROW + 1) + sgr_at(
    0, "m", MSG_COL + 1, MSG_ROW + 1
)

tui_env = os.environ.copy()
tui_env["QAQH_DATA_DIR"] = str(DATA)
# 本脚本的全部意义就是在 strict 探针下跑；不允许外部把它关掉。
tui_env["QAQH_HIT_PROBE"] = "strict"

keys = [
    (4.0, b"\x0e"),                       # Ctrl+N 新会话
    (11.0, "帮我选一个方案".encode()),      # 打提示词
    (13.0, b"\x0d"),                      # 回车发出
    # ① 移动序列**跨 chunk 切断**：两次写之间隔 0.1s，crossterm 必须拼回来。
    (19.0, move[:6]),
    (19.1, move[6:]),
    # ② 按下序列也跨 chunk 切断（SGR 解析器最容易在这里漏事件）。
    (20.0, press[:7]),
    (20.1, press[7:]),
    (20.6, release),
    # ③ 正例：点消息行确实能开菜单（证明下面的反例判据不是"永远看不到菜单"）。
    (28.0, msg_click),
    (29.0, b"\x1b"),                      # Esc 关菜单
    # ④ 合批：Ctrl+L 切路由 + 同一次写里的旧坐标点击。键让帧失效，点击必须被丢。
    (30.0, b"\x0c" + msg_click),
    (31.0, b"\x1b"),                      # Esc 回到 Agent View
    # ⑤ 合批：滚轮 + 同一次写里的消息行点击（滚动让帧失效，点击必须被丢）。
    (33.0, wheel_up + msg_click),
    # ⑥ 再来一次滚轮：保证「回到最新消息」按钮被画出来。
    (34.0, wheel_up),
]
driver = [
    "python3",
    str(REPO_ROOT / "scripts" / "lib" / "pty-driver.py"),
    "--tui", os.environ["TUI"],
    "--raw", str(D / "tui.raw"),
    "--seconds", str(RUN_SECS),
    "--resize", "32:30x110",
    "--exit-code-file", str(D / "tui.exit"),
    "--quit",
]
for at, payload in keys:
    driver += ["--key", f"{at}:{hexed(payload)}"]

try:
    subprocess.run(
        driver,
        cwd=WORK,
        env=tui_env,
        check=True,
        timeout=RUN_SECS + 40,
    )
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
python3 - "$D" "$REPO_ROOT" <<'__PY_CHECK__'
import json
import pathlib
import sys

D = pathlib.Path(sys.argv[1])
REPO_ROOT = pathlib.Path(sys.argv[2])
CLICKED = "方案乙"
DEFAULT_OPTION = "方案甲"
FINAL_TAIL = "第 50 行"
MENU_TITLE = "消息操作"
BACK_TO_LATEST = "回到最新消息"

sys.path.insert(0, str(REPO_ROOT / "scripts" / "lib"))
from vt_screen import render_text

raw_bytes = (D / "tui.raw").read_bytes()
raw = raw_bytes.decode(errors="replace")
screen = render_text(raw)
requests = json.loads((D / "provider-requests.json").read_text(encoding="utf-8"))
exit_code = (D / "tui.exit").read_text().strip() if (D / "tui.exit").exists() else "?"

# ① 跨 chunk 的 SGR 序列仍然命中被点的选项（不是默认第一项）。
answered = any(
    m.get("role") == "tool" and CLICKED in str(m.get("content"))
    for body in requests
    for m in (body.get("messages") or [])
)
wrong_default = any(
    m.get("role") == "tool" and DEFAULT_OPTION in str(m.get("content"))
    for body in requests
    for m in (body.get("messages") or [])
)

# ② 回合在点击之后继续到最终正文：provider 收到第二回合 + 屏幕上能看到正文尾部。
continued = len(requests) >= 2 and FINAL_TAIL in screen

# ③ 正例：点消息行确实开过菜单（否则下面的反例判据没有意义）。
menu_ever_opened = MENU_TITLE in raw

# ④ 反例：滚动 / 切路由之后的旧坐标点击**不得**再开菜单——最终屏幕停在 Agent
#    View，如果那些点击被解释成旧帧，菜单会一直开着。
menu_not_open_at_end = MENU_TITLE not in screen

# ⑤ 滚轮真的滚动了 transcript：只有"可滚且不在底部"时才画「回到最新消息」。
scrolled = BACK_TO_LATEST in screen

# ⑥ strict 探针全程零失败：探针失败会让 TUI 带诊断退出。
probe_clean = (
    exit_code == "0"
    and "hit-probe" not in raw
    and "panicked at" not in raw
    and "QAQH_HIT_PROBE=strict 失败" not in raw
)

# ⑦ 鼠标/焦点捕获进入 / 退出成对。
capture_on = (
    "\x1b[?1000h" in raw
    and "\x1b[?1003h" in raw
    and "\x1b[?1006h" in raw
    and "\x1b[?1004h" in raw
)
capture_off = (
    "\x1b[?1000l" in raw
    and "\x1b[?1003l" in raw
    and "\x1b[?1006l" in raw
    and "\x1b[?1004l" in raw
)

# ⑧ resize 之后仍然正常重排（离开 alternate screen = 干净退出路径走完）。
left_alternate = "\x1b[?1049l" in raw

checks = [
    (f"① 跨 chunk 的 SGR 点击命中目标选项（{CLICKED}）", answered),
    (f"① 反例：没有误提交默认第一项（{DEFAULT_OPTION}）", not wrong_default),
    ("② 点击后回合继续到最终正文（provider 第二回合 + 屏幕正文尾部）", continued),
    ("③ 正例：点消息行确实能打开消息菜单", menu_ever_opened),
    ("④ 反例：滚动/切路由后的旧坐标点击不得再开菜单", menu_not_open_at_end),
    ("⑤ 滚轮真的滚动了 transcript（出现「回到最新消息」）", scrolled),
    ("⑥ QAQH_HIT_PROBE=strict 全程零失败（exit 0 / 无诊断 / 无 panic）", probe_clean),
    ("⑦ fullscreen 鼠标/焦点捕获进入/退出成对", capture_on and capture_off),
    ("⑧ resize 之后仍走完干净退出（离开 alternate screen）", left_alternate),
]
ok = True
for name, hit in checks:
    ok &= hit
    print(f"  [{'✓' if hit else '✗'}] {name}")
if not ok:
    print(f"--- tui exit={exit_code} ---")
    print("--- 重建后的屏幕 ---")
    print(screen[:2000])
print("RESULT:", "PASS" if ok else "FAIL")
sys.exit(0 if ok else 1)
__PY_CHECK__
