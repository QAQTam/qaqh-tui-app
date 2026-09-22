#!/bin/bash
# 真机端到端：V2 permission / ask / $PAGER（fake OpenAI provider + 真实 daemon + PTY）。
#
# 不是 UI mock：TUI 连接真实 daemon，daemon 经本地 OpenAI-compatible provider
# permission：收到 exec tool_call，走真实权限引擎并阻塞在 permission modal。
# ask：收到 ask tool_call，走真实 InteractionRequested 并阻塞在 ask modal。
#
# 用法：
#   MODE=permission scripts/e2e-v2-interactions.sh
#   MODE=ask scripts/e2e-v2-interactions.sh
#   MODE=pager scripts/e2e-v2-interactions.sh
# 前置：qaqh-daemon 与 qaqh-tui 已构建。

set -u

SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
REPO_ROOT=$(cd -- "$SCRIPT_DIR/.." && pwd)
BACKEND_ROOT=${QAQH_BACKEND_ROOT:-$REPO_ROOT/../qaqh-backend}
DAEMON=${DAEMON:-$BACKEND_ROOT/target/debug/qaqh-daemon}
TUI=${TUI:-$REPO_ROOT/target/debug/qaqh-tui}
D=${D:-/tmp/qaqh-e2e-v2-permission}

for binary in "$DAEMON" "$TUI"; do
    [ -x "$binary" ] || {
        echo "缺少可执行文件：$binary（先 cargo build）" >&2
        exit 1
    }
done

case "$D" in
    /tmp/*) ;;
    *)
        echo "D 必须位于 /tmp 下：$D" >&2
        exit 1
        ;;
esac

rm -rf "$D"
mkdir -p "$D/qaqh"

DAEMON="$DAEMON" TUI="$TUI" D="$D" MODE="${MODE:-permission}" python3 - <<'PY'
import fcntl
import json
import os
import pathlib
import pty
import select
import signal
import struct
import subprocess
import termios
import threading
import time
import uuid
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from urllib.request import Request, urlopen

D = pathlib.Path(os.environ["D"])
DATA = D / "qaqh"
DISCOVERY = DATA / "daemon.json"
RAW = D / "tui.raw"
mode = os.environ.get("MODE", "permission")
if mode not in {"permission", "ask", "pager"}:
    raise SystemExit(f"MODE must be permission|ask|pager, got {mode!r}")

def post_json(url, payload, headers=None):
    body = json.dumps(payload).encode()
    request = Request(url, data=body, method="POST")
    request.add_header("Content-Type", "application/json")
    for key, value in (headers or {}).items():
        request.add_header(key, value)
    with urlopen(request, timeout=10) as response:
        return json.loads(response.read())

class FakeProvider(BaseHTTPRequestHandler):
    def log_message(self, fmt, *args):
        pass

    def do_POST(self):
        if self.path not in ("/v1/chat/completions", "/chat/completions"):
            self.send_error(404)
            return
        length = int(self.headers.get("Content-Length", "0"))
        body = self.rfile.read(length)
        try:
            payload = json.loads(body or b"{}")
        except json.JSONDecodeError:
            payload = {}
        self.server.request_count += 1
        first_request = self.server.request_count == 1

        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.send_header("Cache-Control", "no-cache")
        self.end_headers()

        slow_first = False
        if not first_request:
            chunks = [
                {
                    "choices": [
                        {
                            "index": 0,
                            "delta": {"role": "assistant", "content": "permission test complete"},
                            "finish_reason": None,
                        }
                    ]
                },
                {"choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}]},
            ]
        elif mode == "pager":
            slow_first = True
            chunks = [
                {
                    "choices": [
                        {
                            "index": 0,
                            "delta": {
                                "role": "assistant",
                                "reasoning_content": "pager reasoning body",
                            },
                            "finish_reason": None,
                        }
                    ]
                },
                {
                    "choices": [
                        {
                            "index": 0,
                            "delta": {"content": "pager test complete"},
                            "finish_reason": None,
                        }
                    ]
                },
                {"choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}]},
            ]
        else:
            if mode == "permission":
                tool_name = "exec"
                arguments = json.dumps({"command": "echo permission-test"})
            else:
                tool_name = "ask"
                arguments = json.dumps(
                    {
                        "question": "Continue with the test?",
                        "options": ["Yes", "No"],
                        "allow_custom": False,
                    }
                )
            chunks = [
                {
                    "choices": [
                        {"index": 0, "delta": {"role": "assistant"}, "finish_reason": None}
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
                                        "id": "call_permission_1",
                                        "type": "function",
                                        "function": {"name": tool_name, "arguments": arguments},
                                    }
                                ]
                            },
                            "finish_reason": None,
                        }
                    ]
                },
                {
                    "choices": [
                        {"index": 0, "delta": {}, "finish_reason": "tool_calls"}
                    ]
                },
            ]

        for index, chunk in enumerate(chunks):
            self.wfile.write(f"data: {json.dumps(chunk)}\n\n".encode())
            self.wfile.flush()
            time.sleep(5.0 if slow_first and index == 0 else 0.02)
        self.wfile.write(b"data: [DONE]\n\n")
        self.wfile.flush()

server = ThreadingHTTPServer(("127.0.0.1", 0), FakeProvider)
server.request_count = 0
port = server.server_address[1]
threading.Thread(target=server.serve_forever, daemon=True).start()
print(f"fake provider: http://127.0.0.1:{port}/v1")

config = f'''provider_id = "openai"
active_profile = "default"
permission_level = 2

[profiles.default]
model = "fake-model"
max_tokens = 4096
effort = "low"
context_limit = 100000
base_url = "http://127.0.0.1:{port}/v1"
endpoint = "openai"
'''
(DATA / "config.toml").write_text(config)

daemon_log = (D / "daemon.out").open("ab")
daemon_env = os.environ.copy()
daemon_env["QAQH_DATA_DIR"] = str(DATA)
daemon = subprocess.Popen(
    [os.environ["DAEMON"], "run"],
    stdin=subprocess.DEVNULL,
    stdout=daemon_log,
    stderr=subprocess.STDOUT,
    env=daemon_env,
    start_new_session=True,
)

deadline = time.monotonic() + 20
while time.monotonic() < deadline:
    try:
        discovery = json.loads(DISCOVERY.read_text())
    except (OSError, json.JSONDecodeError):
        time.sleep(0.2)
        continue
    if discovery.get("pid") == daemon.pid:
        break
else:
    raise RuntimeError("daemon did not publish daemon.json")
print(f"daemon pid={daemon.pid}")

endpoint = discovery["endpoint"].rstrip("/")
token = discovery["token"]
client_instance_id = str(uuid.uuid4())
open_payload = {
    "schema": "qaqh.Ringing",
    "version": 1,
    "client_instance_id": client_instance_id,
}
opened = post_json(
    f"{endpoint}/ringing/v1/clients/open",
    open_payload,
    {"Authorization": f"Bearer {token}"},
)
client_session_id = opened["client_session_id"]
command_id = str(uuid.uuid4())
command_payload = {
    "schema": "qaqh.Ringing",
    "version": 1,
    "channel": "control",
    "command_id": command_id,
    "client_instance_id": client_instance_id,
    "client_session_id": client_session_id,
    "command": {
        "channel": "control",
        "type": "session_create",
        "close_current": False,
        "cwd": None,
        "tool_mode": None,
        "custom_tools": [],
    },
}
post_json(
    f"{endpoint}/ringing/v1/commands/control",
    command_payload,
    {
        "Authorization": f"Bearer {token}",
        "X-QAQH-Client-Session-Id": client_session_id,
    },
)

sessions_root = DATA / "sessions"
deadline = time.monotonic() + 20
created_seed = None
while time.monotonic() < deadline:
    if sessions_root.is_dir():
        for meta in sessions_root.glob("*/meta.json"):
            try:
                payload = json.loads(meta.read_text())
            except (OSError, json.JSONDecodeError):
                continue
            created_seed = payload.get("seed")
            if created_seed:
                break
    if created_seed:
        break
    time.sleep(0.2)
if not created_seed:
    raise RuntimeError("session_create did not produce a session")
print(f"created seed={created_seed}")

master, slave = pty.openpty()
fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 40, 130, 0, 0))
tui_env = os.environ.copy()
tui_env["TERM"] = "xterm-256color"
tui_env["QAQH_DATA_DIR"] = str(DATA)
tui_env["PAGER"] = "cat"
tui = subprocess.Popen(
    [os.environ["TUI"], "--v2-agent", "--no-spawn"],
    stdin=slave,
    stdout=slave,
    stderr=slave,
    env=tui_env,
    start_new_session=True,
    close_fds=True,
)
os.close(slave)
os.set_blocking(master, False)

capture = bytearray()
query_tail = bytearray()
start = time.monotonic()
queries = 0
sent_open = False
sent_prompt = False
sent_message = False
resolved = False
resolved_at = None
sent_thinking = False
sent_pager = False
quit_sent = False
deadline = start + 35

while time.monotonic() < deadline:
    elapsed = time.monotonic() - start
    if not sent_open and elapsed >= 3.0:
        os.write(master, b"\x0c")
        sent_open = True
    if sent_open and not sent_prompt and elapsed >= 5.0:
        os.write(master, b"\r")
        sent_prompt = True
    if sent_prompt and not sent_message and elapsed >= 9.0:
        os.write(master, b"run permission test\r")
        sent_message = True
    if mode == "permission":
        if b"\xe6\x89\xb9\xe5\x87\x86" in capture and not resolved:
            os.write(master, b"a")
            resolved = True
            resolved_at = time.monotonic()
            print("permission modal detected; approved with a")
    elif mode == "ask":
        if b"Continue with the test?" in capture and not resolved:
            os.write(master, b"1")
            resolved = True
            resolved_at = time.monotonic()
            print("ask modal detected; answered with 1")
    elif mode == "pager":
        if not sent_thinking and elapsed >= 12.0:
            os.write(master, b"\x14")
            sent_thinking = True
        if sent_thinking and not sent_pager and elapsed >= 13.0:
            os.write(master, b"e")
            sent_pager = True
        if sent_pager and not quit_sent and elapsed >= 18.0:
            os.write(master, b"\x11")
            quit_sent = True
            print("Ctrl+Q sent after pager")
    if resolved_at is not None and not quit_sent and time.monotonic() - resolved_at >= 2.0:
        os.write(master, b"\x11")
        quit_sent = True
        print("Ctrl+Q sent after interaction resolved")

    ready, _, _ = select.select([master], [], [], 0.03)
    if ready:
        try:
            chunk = os.read(master, 65536)
        except (BlockingIOError, OSError):
            chunk = b""
        if chunk:
            capture.extend(chunk)
            query_tail.extend(chunk)
            while b"\x1b[6n" in query_tail:
                index = query_tail.index(b"\x1b[6n")
                del query_tail[: index + 4]
                queries += 1
                os.write(master, b"\x1b[1;1R")

    if tui.poll() is not None:
        break

if tui.poll() is None:
    os.killpg(tui.pid, signal.SIGTERM)
    try:
        tui.wait(timeout=2)
    except subprocess.TimeoutExpired:
        os.killpg(tui.pid, signal.SIGKILL)

os.killpg(daemon.pid, signal.SIGKILL)
daemon.wait(timeout=5)
server.shutdown()

RAW.write_bytes(capture)
print(f"tui exit={tui.returncode} bytes={len(capture)} cursor_queries={queries}")

raw = bytes(capture)
if mode == "permission":
    checks = [
        ("permission modal visible", b"\xe6\x89\xb9\xe5\x87\x86" in raw),
        ("permission approved", resolved),
    ]
elif mode == "ask":
    checks = [
        ("ask modal visible", b"Continue with the test?" in raw),
        ("ask answered", resolved),
    ]
else:
    checks = [
        ("pager body visible", b"pager reasoning body" in raw),
        ("pager returned", quit_sent),
    ]
checks += [
    ("no cursor-position timeout", b"cursor position could not be read" not in raw),
    ("no panic", b"panicked at" not in raw),
]
ok = tui.returncode == 0
for name, passed in checks:
    ok &= passed
    print(f"  [{'✓' if passed else '✗'}] {name}")
print("RESULT:", "PASS" if ok else "FAIL")
raise SystemExit(0 if ok else 1)
PY
