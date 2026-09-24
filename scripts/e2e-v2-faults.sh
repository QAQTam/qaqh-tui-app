#!/bin/bash
# 真机端到端：V2 故障注入钩子（fake provider + 真实 daemon + 真实 PTY）。
#
# 与 `e2e-v2-interactions.sh` 的分工：那个脚本覆盖 permission / ask / plan 的
# **交互路径**；这个覆盖 **SSE / timeline / 命令面** 的故障与恢复。
#
# 钩子契约：后端 `docs/spec/2026-09-23-TUI契约测试钩子-spec.md` §2 / §3。
# 所有开关都是 daemon **启动时读一次**（除 `QAQH_TEST_LEASE_TTL_MS`）。
#
# 用法：
#   MODE=lagged       SSE_TERMINATE=lagged（channel 作用域）→ U-07 诊断 + 重连后仍可用
#   MODE=gap          TIMELINE_GAP=1 → 客户端 re-baseline，被丢弃的 entry 必须补回
#   MODE=ack-delay    COMMAND_ACK=1500（conversation）→ 慢 ack 不影响回合完成
#   MODE=ack-hang     COMMAND_ACK=hang（conversation）→ 在途命令不卡 UI，可干净退出
#   MODE=session-404  SESSION_404_SEED=* → 非子代理 404 只提示、不关会话（U-29 判据）
# 前置：qaqh-daemon 与 qaqh-tui 已构建。
#
# ⚠ 锚点口径（2026-09-24 更新）：
#   - 当前 TUI pin / 本机 anchor 已是
#     `tui-ringing-v2-interaction-causation-2026-09-24` @ `b77c251`；
#   - 上一版 `tui-ringing-v2-types-2026-09-24` @ `a43a8bc` 六模式实测全绿；
#   - 旧冻结锚点 `b40ff698` 仍不含后端修复：`none` / `lagged` / `gap` /
#     `ack-delay` / `ack-hang` 五个模式会红，这是预期；
#   - 验证记录见
#     `docs/report/2026-09-24-backend-main-anchor-verification-report.md`。

set -u

SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
REPO_ROOT=$(cd -- "$SCRIPT_DIR/.." && pwd)
# 默认吃**锚点 worktree**（TUI 钉的 rev，见 scripts/ci-linux.sh 的 QAQH_BACKEND_REV），
# 而不是开发者正在用的 ../qaqh-backend 工作树——否则 e2e 会跑到别人分支构建的
# daemon 上，与本仓门禁的锚点不是同一个东西。
BACKEND_ROOT=${QAQH_BACKEND_ROOT:-$REPO_ROOT/../qaqh-backend-anchor}
DAEMON=${DAEMON:-$BACKEND_ROOT/target/debug/qaqh-daemon}
TUI=${TUI:-$REPO_ROOT/target/debug/qaqh-tui}
MODE="${MODE:-lagged}"
# 每个 MODE 用各自的隔离 data root，避免不同模式之间（或并行跑）互相踩。
D=${D:-/tmp/qaqh-e2e-v2-fault-$MODE}

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

DAEMON="$DAEMON" TUI="$TUI" D="$D" MODE="$MODE" python3 - <<'PY'
import json
import os
import pathlib
import pty
import fcntl
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
mode = os.environ.get("MODE", "lagged")
if mode not in {"none", "lagged", "gap", "ack-delay", "ack-hang", "session-404"}:
    raise SystemExit(
        "MODE must be none|lagged|gap|ack-delay|ack-hang|session-404, "
        f"got {mode!r}"
    )

# 每个 MODE 要注入的 daemon 开关（钩子 spec §2/§3）。
# 注意 `COMMAND_ACK_COMMAND` 默认是 `interaction`（只命中 permission/ask 响应）；
# 这里要测的是**普通会话命令**，所以显式限定到 conversation 频道。
FAULT_ENV = {
    "none": {},
    "lagged": {
        "QAQH_TEST_SSE_TERMINATE": "lagged",
        # 必须落在 **timeline** 作用域：channel 三条流在 TUI 启动时就建好了，
        # 那时还没有任何会话标签，`status_line` 的 conn_error 只在
        # `active_session()` 存在时渲染 → 启动期的 channel 告警在 v2 里看不见。
        # timeline 流是**打开会话时**才 activate 的，正好在标签已存在之后。
        "QAQH_TEST_SSE_TERMINATE_SCOPE": "timeline",
    },
    "gap": {"QAQH_TEST_TIMELINE_GAP": "1"},
    "ack-delay": {
        "QAQH_TEST_COMMAND_ACK": "1500",
        "QAQH_TEST_COMMAND_ACK_CHANNEL": "conversation",
        "QAQH_TEST_COMMAND_ACK_COMMAND": "all",
    },
    "ack-hang": {
        "QAQH_TEST_COMMAND_ACK": "hang",
        "QAQH_TEST_COMMAND_ACK_CHANNEL": "conversation",
        "QAQH_TEST_COMMAND_ACK_COMMAND": "all",
    },
    "session-404": {"QAQH_TEST_SESSION_404_SEED": "*"},
}

ASSISTANT_TEXT = "fault test complete"
# 用户消息必须落在 transcript 里（TurnOpened 的 user_text）——gap 模式若
# re-baseline 失效，最先丢的就是它。
USER_TEXT = "run fault test"


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
        self.rfile.read(length)
        self.server.request_count += 1

        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.send_header("Cache-Control", "no-cache")
        self.end_headers()
        chunks = [
            {
                "choices": [
                    {
                        "index": 0,
                        "delta": {"role": "assistant", "content": ASSISTANT_TEXT},
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
server.request_count = 0
port = server.server_address[1]
threading.Thread(target=server.serve_forever, daemon=True).start()

config = f'''provider_id = "openai"
active_profile = "default"
permission_level = 1

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
for key, value in FAULT_ENV[mode].items():
    daemon_env[key] = value
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
print(f"daemon pid={daemon.pid} mode={mode} faults={FAULT_ENV[mode]}")

endpoint = discovery["endpoint"].rstrip("/")
token = discovery["token"]
client_instance_id = str(uuid.uuid4())
opened = post_json(
    f"{endpoint}/ringing/v1/clients/open",
    {
        "schema": "qaqh.Ringing",
        "version": 1,
        "client_instance_id": client_instance_id,
    },
    {"Authorization": f"Bearer {token}"},
)
client_session_id = opened["client_session_id"]
# 建会话本身走 control 频道：`ack-*` 模式把故障限定在 conversation，
# 所以这一步不受影响（否则脚本自己就会卡在建会话上）。
post_json(
    f"{endpoint}/ringing/v1/commands/control",
    {
        "schema": "qaqh.Ringing",
        "version": 1,
        "channel": "control",
        "command_id": str(uuid.uuid4()),
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
    },
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
quit_sent = False
deadline = start + 30

while time.monotonic() < deadline:
    elapsed = time.monotonic() - start
    # 会话列表 → 打开会话 → 发一条消息（各模式都要走完这段真实路径）。
    if not sent_open and elapsed >= 3.0:
        os.write(master, b"\x0c")
        sent_open = True
    if sent_open and not sent_prompt and elapsed >= 5.0:
        os.write(master, b"\r")
        sent_prompt = True
    if sent_prompt and not sent_message and elapsed >= 9.0:
        os.write(master, (USER_TEXT + "\r").encode())
        sent_message = True
    if not quit_sent and elapsed >= 22.0:
        os.write(master, b"\x11")
        quit_sent = True

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

if daemon.poll() is None:
    os.killpg(daemon.pid, signal.SIGKILL)
    daemon.wait(timeout=5)
server.shutdown()

RAW.write_bytes(capture)
print(
    f"tui exit={tui.returncode} bytes={len(capture)} "
    f"cursor_queries={queries} provider_requests={server.request_count}"
)

raw = bytes(capture)
user_seen = USER_TEXT.encode() in raw
reply_seen = ASSISTANT_TEXT.encode() in raw

if mode == "none":
    checks = [
        ("基线：用户消息可见", user_seen),
        ("基线：回复可见", reply_seen),
    ]
elif mode == "lagged":
    # U-07：诊断文案必须可见（`runtime.rs::reconnect_message` 的 lagged 分支），
    # 且重连之后会话仍然可用（回复照常出现）。
    checks = [
        ("lagged 诊断文案可见", b"lagged" in raw),
        ("重连后会话仍可用（回复可见）", reply_seen),
    ]
elif mode == "gap":
    # gap 钩子丢掉的是**第一条真实 entry**（通常是 TurnOpened）。若客户端
    # re-baseline 失效，用户消息会先消失——所以两条都要断言。
    checks = [
        ("gap 后用户消息仍在（re-baseline 生效）", user_seen),
        ("gap 后回复可见", reply_seen),
    ]
elif mode == "ack-delay":
    # 慢 ack 只延迟命令处理，回合照常完成。
    checks = [
        ("慢 ack 下回复仍出现", reply_seen),
        ("用户消息可见", user_seen),
    ]
elif mode == "ack-hang":
    # 命令被永久冻结 → 不该有回复；但 UI 不能卡死，退出必须干净。
    checks = [
        ("被冻结的命令未产生回复", not reply_seen),
        ("Ctrl+Q 后干净退出", quit_sent and tui.returncode == 0),
    ]
else:  # session-404
    # U-29 判据：非子代理的 404 只提示、不关会话。
    #
    # ⚠ 文案对齐（2026-09-23）：钩子拦的是 **bootstrap** 请求，TUI 走的是
    # `bootstrap 失败[seed]: HTTP 404: …` 这条提示路径；而
    # `TimelineLostReason::SessionMissing` 的「会话不存在（404）」属于
    # **timeline 流** 404 的另一条路径，本场景不会出现。原断言写的是后者，
    # 属于「断言了错误的代码路径」——行为断言（不关会话）一直是绿的。
    checks = [
        (
            "404 提示可见（bootstrap 失败 + HTTP 404）",
            b"bootstrap" in raw and b"404" in raw,
        ),
        ("非子会话未被关闭（仍能干净退出）", quit_sent and tui.returncode == 0),
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
