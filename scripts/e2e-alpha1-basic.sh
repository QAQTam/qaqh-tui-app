#!/bin/bash
# 真机端到端：alpha1「基本可用」验收。
#
# 上边的验收口径是「前端侧显示 chat 和工具基本正常，后端基本功能比如编辑/读写/
# bash 等工具正常」。本脚本把这句话变成可复现判据：**一个真实回合**里让 agent
# 依次调用 `write` → `read` → `edit` → `exec`（bash），四个工具都真的落到文件系统
# 上，且 TUI 把 chat 正文与工具调用/结果都画出来。
#
# 为什么用 fake provider：本机没有任何真实 provider 凭据，而「模型会不会调工具」
# 不是本仓能控的变量。这里用一个本地 OpenAI-compatible provider **按脚本**下发
# tool_calls —— 链路（TUI → daemon → provider → 工具执行 → 回灌 → TUI 渲染）全真，
# 只有"模型说什么"是脚本化的。与 `e2e-v2-interactions.sh` 同一套路。
#
# 判据（缺一即红）：
#   ① 前端：屏幕出现最终 assistant 正文；
#   ② 前端：屏幕出现四个工具名（write / read / edit / exec）；
#   ③ 后端：文件真的被 write → edit 成 v2 内容（读盘判据，不信 UI）；
#   ④ 后端：provider 上下文里出现 role=tool 的结果（工具结果真的回灌给了模型）。
#
# 用法：scripts/e2e-alpha1-basic.sh
# 前置：两仓均已 `cargo build`。全程用隔离 data root + 隔离 workspace。

set -u
SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
REPO_ROOT=$(cd -- "$SCRIPT_DIR/.." && pwd)
# 默认吃**锚点 worktree**（TUI 钉的 rev，见 scripts/ci-linux.sh 的 QAQH_BACKEND_REV）。
BACKEND_ROOT=${QAQH_BACKEND_ROOT:-$REPO_ROOT/../qaqh-backend}
DAEMON=${DAEMON:-$BACKEND_ROOT/target/debug/qaqh-daemon}
TUI=${TUI:-$REPO_ROOT/target/debug/qaqh-tui}
D=${D:-/tmp/qaqh-e2e-alpha1-basic}
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

echo "跑 alpha1 基本可用回合（隔离 data root + workspace）…"
DAEMON="$DAEMON" TUI="$TUI" REPO_ROOT="$REPO_ROOT" DATA="$DATA" WORK="$WORK" \
  RUN_SECS="$RUN_SECS" python3 - <<'PY'
import json
import os
import pathlib
import subprocess
import threading
import time
import uuid
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

D = pathlib.Path(os.environ["DATA"]).parent
DATA = pathlib.Path(os.environ["DATA"])
WORK = pathlib.Path(os.environ["WORK"])
REPO_ROOT = pathlib.Path(os.environ["REPO_ROOT"])
RUN_SECS = float(os.environ["RUN_SECS"])
TARGET = WORK / "alpha1-basic.txt"
TARGET_STR = str(TARGET)
FINAL_TEXT = "alpha1 基本可用：write/read/edit/exec 全通"

# 模型侧脚本：依次下发四个工具调用，最后给一句正文。
# 参数名与 daemon 真实下发给模型的 schema 一致（探针实测，见 tools 数组）。
SCRIPT = [
    ("write", {"path": TARGET_STR, "content": "alpha1-basic-v1\n"}),
    ("read", {"path": TARGET_STR}),
    ("edit", {"path": TARGET_STR, "old_str": "alpha1-basic-v1", "new_str": "alpha1-basic-v2"}),
    ("exec", {"command": "cat alpha1-basic.txt"}),
]


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

        # 标题生成器是**非流式**的旁路请求（daemon 用它概括首条消息），
        # 不参与工具脚本计数。
        if not payload.get("stream"):
            body = json.dumps(
                {
                    "choices": [
                        {"message": {"role": "assistant", "content": "基本可用冒烟"}}
                    ]
                }
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

        if turn < len(SCRIPT):
            name, args = SCRIPT[turn]
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
                                        "id": f"call_alpha1_{turn}",
                                        "type": "function",
                                        "function": {
                                            "name": name,
                                            "arguments": json.dumps(args),
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
            # 最后正文分多个 SSE delta 下发：既验收 chat 正文存在，也覆盖
            # V2 的稳定行流式提交路径（不是只在 turn sealed 时一次性蹦出）。
            chunks = [
                {
                    "choices": [
                        {
                            "index": 0,
                            "delta": {"role": "assistant", "content": "alpha1 "},
                            "finish_reason": None,
                        }
                    ]
                },
                {
                    "choices": [
                        {
                            "index": 0,
                            "delta": {"content": "基本可用："},
                            "finish_reason": None,
                        }
                    ]
                },
                {
                    "choices": [
                        {
                            "index": 0,
                            "delta": {"content": "write/read/edit/exec "},
                            "finish_reason": None,
                        }
                    ]
                },
                {
                    "choices": [
                        {
                            "index": 0,
                            "delta": {"content": "全通"},
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

# permission_level = 3（WorkspaceFree，daemon 默认）：workspace 内的
# write/edit/exec 自动放行，不会弹授权 modal 把回合卡住。
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

# 真 PTY 驱动；TUI 的 cwd = 隔离 workspace（会话 workspace 由此而来）。
# Ctrl+N 建会话（约 3-6s 后自动打开），再输入一句 prompt 回车。
tui_env = os.environ.copy()
tui_env["QAQH_DATA_DIR"] = str(DATA)
driver = [
    "python3",
    str(REPO_ROOT / "scripts" / "lib" / "pty-driver.py"),
    "--tui", os.environ["TUI"],
    "--raw", str(D / "tui.raw"),
    "--seconds", str(RUN_SECS),
    "--key", "4:0e",
    "--type", "12:请依次写入、读取、编辑这个文件，再用 bash 打印它",
    "--key", "14:0d",
    # `exec` 在 level 3 下仍要确认（"requires execution or network confirmation"）。
    # 出现权限 modal 就按 `a` 批准 —— 这条也顺带把"用户能批准工具授权"纳入验收。
    "--respond", "工具权限:61",
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
python3 - "$D" "$REPO_ROOT" "$WORK" <<'PY'
import json
import pathlib
import re
import sys

D = pathlib.Path(sys.argv[1])
REPO_ROOT = pathlib.Path(sys.argv[2])
WORK = pathlib.Path(sys.argv[3])
TARGET = WORK / "alpha1-basic.txt"

sys.path.insert(0, str(REPO_ROOT / "scripts" / "lib"))
from vt_screen import render_text

raw = (D / "tui.raw").read_text(errors="replace")
screen = render_text(raw)
# 流式提交后，已稳定行可能已经滚出最终可见 viewport；验收要看终端原始
# 写入流里的 chat 正文，而不是只盯最终一屏（后者会漏掉 scrollback）。
raw_clean = re.sub(r"\x1b\[[0-9;?]*[a-zA-Z]", "", raw)
raw_clean = re.sub(r"\x1b\][^\x07\x1b]*(?:\x07|\x1b\\)", "", raw_clean)
frontend_chat = all(part in raw_clean for part in ("alpha1", "write/read/edit/exec", "全通"))
requests = json.loads((D / "provider-requests.json").read_text(encoding="utf-8"))

on_disk = TARGET.read_text() if TARGET.exists() else ""
tool_fed_back = any(
    m.get("role") == "tool" and "alpha1-basic-v2" in str(m.get("content"))
    for body in requests
    for m in (body.get("messages") or [])
)

checks = [
    ("① 前端显示 chat 正文", frontend_chat),
    ("② 前端显示四个工具名",
     all(name in screen for name in ("write", "read", "edit", "exec"))),
    ("③ 后端 write→edit 落盘为 v2", on_disk == "alpha1-basic-v2\n"),
    ("④ 工具结果回灌 provider（role=tool）", tool_fed_back),
]
ok = True
for name, hit in checks:
    ok &= hit
    print(f"  [{'✓' if hit else '✗'}] {name}")
if not ok:
    print("--- 重建后的屏幕 ---")
    print(screen[:2000])
    print(f"--- 磁盘内容: {on_disk!r}")
print("RESULT:", "PASS" if ok else "FAIL")
sys.exit(0 if ok else 1)
PY
