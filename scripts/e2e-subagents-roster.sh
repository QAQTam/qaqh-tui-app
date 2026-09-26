#!/bin/bash
# 真机端到端：Team roster 的 spawn → running/loaded → completed/unloaded 可见性。
#
# 用法：scripts/e2e-subagents-roster.sh
# 前置：两仓均已 `cargo build`（脚本不构建）。
# 隔离：私有 QAQH_DATA_DIR（/tmp 下）+ 私有 fake provider，不碰在用的 daemon。

set -u
SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
REPO_ROOT=$(cd -- "$SCRIPT_DIR/.." && pwd)
BACKEND_ROOT=${QAQH_BACKEND_ROOT:-$REPO_ROOT/../qaqh-backend}
DAEMON=${DAEMON:-$BACKEND_ROOT/target/debug/qaqh-daemon}
TUI=${TUI:-$REPO_ROOT/target/debug/qaqh-tui}
D=${D:-/tmp/qaqh-e2e-subagents-roster}
RUN_SECS=${RUN_SECS:-50}

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

echo "跑 Team roster PTY 场景（隔离 data root + fake provider）…"
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
CHILD_TASK = "ROSTER_CHILD_TASK"
CHILD_NAME = "roster_probe"


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
                {"choices": [{"message": {"role": "assistant", "content": "ok"}}]}
            ).encode()
            self.send_response(200)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)
            return

        messages = payload.get("messages") or []
        has_tool_result = any(message.get("role") == "tool" for message in messages)
        is_child = (not has_tool_result) and any(
            CHILD_TASK in str(message.get("content", "")) for message in messages
        )

        if is_child:
            self.server.child_calls += 1
            # Keep the child resident long enough for the first `/subagents`
            # observation to see running/loaded before the completion delta.
            time.sleep(20.0)
            chunks = [
                {
                    "choices": [
                        {
                            "index": 0,
                            "delta": {"role": "assistant", "content": "child completed"},
                            "finish_reason": None,
                        }
                    ]
                },
                {"choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}]},
            ]
        elif has_tool_result:
            self.server.main_calls += 1
            chunks = [
                {
                    "choices": [
                        {
                            "index": 0,
                            "delta": {"role": "assistant", "content": "parent received child result"},
                            "finish_reason": None,
                        }
                    ]
                },
                {"choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}]},
            ]
        else:
            self.server.main_calls += 1
            chunks = [
                {
                    "choices": [
                        {
                            "index": 0,
                            "delta": {"role": "assistant"},
                            "finish_reason": None,
                        }
                    ]
                },
                {
                    "choices": [
                        {
                            "index": 0,
                            "delta": {
                                "tool_calls": [
                                    {
                                        "index": 0,
                                        "id": "call_spawn_roster",
                                        "type": "function",
                                        "function": {
                                            "name": "spawn_subagent",
                                            "arguments": json.dumps(
                                                {
                                                    "agent_name": CHILD_NAME,
                                                    "task_description": CHILD_TASK,
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

        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.send_header("Cache-Control", "no-cache")
        self.end_headers()
        for chunk in chunks:
            self.wfile.write(f"data: {json.dumps(chunk)}\n\n".encode())
            self.wfile.flush()
            time.sleep(0.02)
        self.wfile.write(b"data: [DONE]\n\n")
        self.wfile.flush()


server = ThreadingHTTPServer(("127.0.0.1", 0), FakeProvider)
server.main_calls = 0
server.child_calls = 0
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

tui_env = os.environ.copy()
tui_env["QAQH_DATA_DIR"] = str(DATA)
tui_env["QAQH_HIT_PROBE"] = "strict"

# Timeline: create session, ask the provider to spawn a child, observe the
# running roster, close it, then observe the completed/unloaded roster.
keys = [
    (4.0, b"\x0e"),                 # Ctrl+N
    (8.0, b"spawn a child"),         # prompt
    (10.0, b"\x0d"),                 # send
    (12.0, b"a"),                    # approve spawn_subagent permission
    # Keep the roster open while the child transitions; the same overlay is
    # redrawn by TeamDelta, so one PTY run captures both states.
    (26.0, b"/subagents"),
    (27.0, b"\x0d"),
]
driver = [
    "python3",
    str(REPO_ROOT / "scripts" / "lib" / "pty-driver.py"),
    "--tui", os.environ["TUI"],
    "--raw", str(D / "tui.raw"),
    "--seconds", str(RUN_SECS),
    "--exit-code-file", str(D / "tui.exit"),
    "--quit",
]
for at, payload in keys:
    driver += ["--key", f"{at}:{payload.hex()}"]

try:
    subprocess.run(driver, cwd=WORK, env=tui_env, check=True, timeout=RUN_SECS + 40)
finally:
    try:
        daemon.terminate()
        daemon.wait(timeout=5)
    except Exception:
        daemon.kill()

(D / "provider-requests.json").write_text(
    json.dumps(server.requests, ensure_ascii=False, indent=2), encoding="utf-8"
)
print(f"provider main/child calls: {server.main_calls}/{server.child_calls}")
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
CHILD_PATH = "/root/roster_probe"
sys.path.insert(0, str(REPO_ROOT / "scripts" / "lib"))
from vt_screen import render_text

raw_bytes = (D / "tui.raw").read_bytes()
raw = raw_bytes.decode(errors="replace")
requests = json.loads((D / "provider-requests.json").read_text(encoding="utf-8"))
exit_code = (D / "tui.exit").read_text().strip() if (D / "tui.exit").exists() else "?"
final_screen = render_text(raw)

spawned = any("spawn_subagent" in json.dumps(body) for body in requests)
child_called = sum(
    1
    for body in requests
    if any("ROSTER_CHILD_TASK" in str(message.get("content", "")) for message in (body.get("messages") or []))
) >= 1

# Reconstruct every frame where the roster title was emitted. The first open
# observes the live child; the second open observes the retained unloaded entry.
positions = []
start = 0
while True:
    index = raw_bytes.find(b"Subagents", start)
    if index < 0:
        break
    positions.append(index)
    start = index + 1
frames = [
    render_text(raw_bytes[: position + 2400].decode(errors="replace"))
    for position in positions
]
# The first open happens while the fake child provider is still sleeping. Use
# a shorter prefix than the terminal-state check so a later completion redraw
# cannot overwrite the live `running loaded` frame.
live_frames = [
    render_text(raw_bytes[: position + 1000].decode(errors="replace"))
    for position in positions
]
loaded_seen = any(CHILD_PATH in frame and "loaded" in frame for frame in frames)
running_loaded_seen = any(
    CHILD_PATH in frame and "running" in frame and "loaded" in frame
    for frame in live_frames
)
unloaded_seen = any(
    CHILD_PATH in frame and "completed" in frame and "unloaded" in frame
    for frame in frames
)
roster_seen = any("Subagents" in frame and "INBOX" in frame for frame in frames)
clean = exit_code == "0" and "panicked at" not in raw and "hit-probe" not in raw

checks = [
    ("① provider 确实触发了 spawn_subagent", spawned and child_called),
    ("② spawn 后 roster 出现且标 loaded", loaded_seen),
    ("③ live child 状态可见为 running/loaded", running_loaded_seen),
    ("④ child 完成后 roster 仍保留且标 completed/unloaded", unloaded_seen),
    ("⑤ /subagents 同时渲染 roster 与 inbox", roster_seen),
    ("⑥ strict 探针全程干净退出", clean),
]
ok = True
for name, hit in checks:
    ok &= hit
    print(f"  [{'✓' if hit else '✗'}] {name}")
if not ok:
    print(f"--- tui exit={exit_code} ---")
    print("--- 最终屏幕 ---")
    print(final_screen[:3000])
print("RESULT:", "PASS" if ok else "FAIL")
sys.exit(0 if ok else 1)
__PY_CHECK__
