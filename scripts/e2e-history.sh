#!/bin/bash
# 真机端到端：`/history` 全屏回合浏览。
#
# 验的是这条链：两个真实回合 → `/history` 打开全屏 Workspace（alternate screen）
# → 列表按回合列出 → 选中第二个 → Enter 进详情 → 详情里能看到该回合的正文。
#
# 为什么详情内容要单独钉：列表能列出编号不等于"点进去有东西"。详情渲染的是与
# `e 导出此回合`**同一份** Markdown（`export_turn_markdown`），所以这条也顺带
# 保证了"看到的 = 导出的"。
#
# 判据（缺一即缺证）：
#   ① 列表里出现**两个**回合的用户问题（不是只列了个编号）；
#   ② 详情里出现该回合的正文；
#   ③ 详情底部是详情键位（`e 导出此回合`），证明确实进了详情而不是停在列表；
#   ④ 屏幕模式纪律：进出 alternate screen、且鼠标捕获跟着开/关。
#
# 用法：scripts/e2e-history.sh
# 前置：两仓均已 `cargo build`。

set -u
SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
REPO_ROOT=$(cd -- "$SCRIPT_DIR/.." && pwd)
BACKEND_ROOT=${QAQH_BACKEND_ROOT:-$REPO_ROOT/../qaqh-backend-anchor}
DAEMON=${DAEMON:-$BACKEND_ROOT/target/debug/qaqh-daemon}
TUI=${TUI:-$REPO_ROOT/target/debug/qaqh-tui}
D=${D:-/tmp/qaqh-e2e-history}
RUN_SECS=${RUN_SECS:-45}

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

echo "跑 /history 回合（隔离 data root + workspace）…"
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

# 模型输出刻意用 ASCII：断言要跨 CJK 光标分段时容易假红，正文用 ASCII 最稳。
ANSWERS = ["answer-one", "answer-two"]


class FakeProvider(BaseHTTPRequestHandler):
    def log_message(self, fmt, *args):
        pass

    def do_POST(self):
        if self.path not in ("/v1/chat/completions", "/chat/completions"):
            self.send_error(404)
            return
        length = int(self.headers.get("Content-Length", "0"))
        payload = json.loads(self.rfile.read(length) or b"{}")
        if not payload.get("stream"):
            body = json.dumps(
                {"choices": [{"message": {"role": "assistant", "content": "历史冒烟"}}]}
            ).encode()
            self.send_response(200)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)
            return

        turn = self.server.main_calls
        self.server.main_calls += 1
        text = ANSWERS[turn] if turn < len(ANSWERS) else "extra"

        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.send_header("Cache-Control", "no-cache")
        self.end_headers()
        for chunk in [
            {"choices": [{"index": 0, "delta": {"role": "assistant", "content": text}, "finish_reason": None}]},
            {"choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}]},
        ]:
            self.wfile.write(f"data: {json.dumps(chunk)}\n\n".encode())
            self.wfile.flush()
            time.sleep(0.02)
        self.wfile.write(b"data: [DONE]\n\n")
        self.wfile.flush()


server = ThreadingHTTPServer(("127.0.0.1", 0), FakeProvider)
server.main_calls = 0
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

tui_env = os.environ.copy()
tui_env["QAQH_DATA_DIR"] = str(DATA)
driver = [
    "python3",
    str(REPO_ROOT / "scripts" / "lib" / "pty-driver.py"),
    "--tui", os.environ["TUI"],
    "--raw", str(D / "tui.raw"),
    "--seconds", str(RUN_SECS),
    "--key", "4:0e",              # Ctrl+N 建会话
    "--type", "11:first question",
    "--key", "13:0d",             # 第 1 回合
    "--type", "19:second question",
    "--key", "21:0d",             # 第 2 回合
    "--type", "29:/history",
    "--key", "31:0d",             # 打开历史（全屏 Workspace）
    "--key", "34:6a",             # j：选到第 2 个回合
    "--key", "36:0d",             # Enter：进详情
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

print(f"provider 主回合调用次数: {server.main_calls}")
print(f"产物目录: {D}")
PY

echo
echo "== 判据 =="
python3 - "$D" "$REPO_ROOT" <<'PY'
import pathlib
import sys

D = pathlib.Path(sys.argv[1])
REPO_ROOT = pathlib.Path(sys.argv[2])

sys.path.insert(0, str(REPO_ROOT / "scripts" / "lib"))
from vt_screen import render_text

raw_bytes = (D / "tui.raw").read_bytes()
raw = raw_bytes.decode(errors="replace")
screen = render_text(raw)
# 宽字符在重建里可能被空格隔开，按去空白口径比对。
tight = "".join(screen.split())

checks = [
    # 列表：两个回合的用户问题都得在（列表行是单个 Span，字节流里连续）。
    ("① 列表列出第 1 个回合", "first question" in raw),
    ("① 列表列出第 2 个回合", "second question" in raw),
    # 详情：正文 + 详情专属键位提示。
    ("② 详情显示该回合正文", "answer-two" in tight),
    ("③ 进入的是详情视图（e 导出此回合）", "e导出此回合" in tight),
    # 屏幕模式纪律：进/出 alternate screen，鼠标捕获跟着开/关。
    ("④ 进出 alternate screen", "\x1b[?1049h" in raw and "\x1b[?1049l" in raw),
    (
        "④ 鼠标捕获随全屏面开/关",
        "\x1b[?1000h" in raw and "\x1b[?1000l" in raw,
    ),
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
