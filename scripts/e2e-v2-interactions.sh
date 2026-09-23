#!/bin/bash
# 真机端到端：V2 permission / ask / plan / $PAGER（fake OpenAI provider + 真实 daemon + PTY）。
#
# 不是 UI mock：TUI 连接真实 daemon，daemon 经本地 OpenAI-compatible provider
# permission：收到 exec tool_call，走真实权限引擎并阻塞在 permission modal。
# ask：收到 ask tool_call，走真实 InteractionRequested 并阻塞在 ask modal。
# plan：daemon 侧 `QAQH_TEST_PLAN_REVIEW=1`，在 round 0 调用 provider 前生成真实
#       `PlanReviewRequested` 并挂起（后端契约测试钩子 spec §1.1）。
# *-hang：daemon 侧 `QAQH_TEST_INTERACTION_FAULT=<mode>`，应答命令**永不返回 ack**
#       （spec §1.2），用于验证 TUI 的应答超时终态与 daemon 消失后的干净退出。
# permission-deny / ask-dismiss / plan-reject：同上的**否定路径**（`d` 拒绝 / `Esc` 跳过 /
#       `r`+理由+Enter 驳回）。判据不是「模态消失」，而是后端物化 timeline 落到否定终态
#       （tool_denied / cancelled / `Plan rejected: <理由>`）。
#
# 用法：
#   MODE=permission scripts/e2e-v2-interactions.sh
#   MODE=ask scripts/e2e-v2-interactions.sh
#   MODE=plan scripts/e2e-v2-interactions.sh
#   MODE=pager scripts/e2e-v2-interactions.sh
#   MODE=permission-hang scripts/e2e-v2-interactions.sh
#   MODE=ask-hang scripts/e2e-v2-interactions.sh
#   MODE=permission-deny scripts/e2e-v2-interactions.sh
#   MODE=ask-dismiss scripts/e2e-v2-interactions.sh
#   MODE=plan-reject scripts/e2e-v2-interactions.sh
# 前置：qaqh-daemon 与 qaqh-tui 已构建。

set -u

SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
REPO_ROOT=$(cd -- "$SCRIPT_DIR/.." && pwd)
# 默认吃**锚点 worktree**（TUI 钉的 rev，见 scripts/ci-linux.sh 的 QAQH_BACKEND_REV），
# 而不是开发者正在用的 ../qaqh-backend 工作树——否则 e2e 会跑到别人分支构建的
# daemon 上，与本仓门禁的锚点不是同一个东西。
BACKEND_ROOT=${QAQH_BACKEND_ROOT:-$REPO_ROOT/../qaqh-backend-anchor}
DAEMON=${DAEMON:-$BACKEND_ROOT/target/debug/qaqh-daemon}
TUI=${TUI:-$REPO_ROOT/target/debug/qaqh-tui}
MODE="${MODE:-permission}"
# 每个 MODE 用各自的隔离 data root，避免不同模式之间（或并行跑）互相踩。
D=${D:-/tmp/qaqh-e2e-v2-$MODE}

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
import fcntl
import hashlib
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
# `*-hang` 是「同一个交互 + 注入 ack 永不返回」的组合模式，先拆成 base + is_hang。
HANG_BASE = {"permission-hang": "permission", "ask-hang": "ask"}
# 否定路径：同一个交互，按键换成「拒绝 / 跳过 / 拒绝并填理由」。
# 判据不是「模态消失」（那只是 TUI 侧下架），而是**后端 timeline 落到否定终态**。
NEGATIVE = {"permission-deny": "permission", "ask-dismiss": "ask", "plan-reject": "plan"}
if mode not in {"permission", "ask", "plan", "pager", *HANG_BASE, *NEGATIVE}:
    raise SystemExit(
        "MODE must be permission|ask|plan|pager|permission-hang|ask-hang|"
        "permission-deny|ask-dismiss|plan-reject, "
        f"got {mode!r}"
    )
base = HANG_BASE.get(mode) or NEGATIVE.get(mode) or mode
is_hang = mode in HANG_BASE
is_negative = mode in NEGATIVE

def post_json(url, payload, headers=None):
    body = json.dumps(payload).encode()
    request = Request(url, data=body, method="POST")
    request.add_header("Content-Type", "application/json")
    for key, value in (headers or {}).items():
        request.add_header(key, value)
    with urlopen(request, timeout=10) as response:
        return json.loads(response.read())

def get_json(url, headers=None):
    request = Request(url, method="GET")
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
        # 留住**每一次**请求体：否定路径要用「模型上下文里到底有没有
        # 拒绝理由 / 拒绝结果」作后端权威证据（见 `negative_evidence`）。
        self.server.requests.append(payload)
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
        elif base == "pager":
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
            if base == "plan":
                # plan 模式：`PlanReviewRequested` 在 round 0、**调用 provider 之前**
                # 就挂起（spec §1.1），所以走到这里的已经是批准之后的那一轮——
                # 回一段纯文本即可，被测对象是 plan modal 本身。
                chunks = [
                    {
                        "choices": [
                            {
                                "index": 0,
                                "delta": {"role": "assistant", "content": "plan approved"},
                                "finish_reason": None,
                            }
                        ]
                    },
                    {"choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}]},
                ]
            else:
                if base == "permission":
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
server.requests = []
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
# 后端契约测试钩子（后端 `docs/spec/2026-09-23-TUI契约测试钩子-spec.md`）。
# daemon **启动时读一次**；除 QAQH_TEST_LEASE_TTL_MS 外运行中不重读。
if base == "plan":
    daemon_env["QAQH_TEST_PLAN_REVIEW"] = "1"
if is_hang:
    daemon_env["QAQH_TEST_INTERACTION_FAULT"] = mode
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
daemon_killed = False
# plan-reject 是两段式：`r` 进理由输入态，再 `Enter` 提交。
sent_reject_reason = False
reject_entered_at = None
# TUI 侧交互应答的 ack 上限（`INTERACTION_ACK_TIMEOUT`，10s）触发后的 toast 文案。
# 断言用「子串」而不是整句：文案里带秒数，改上限不该让本脚本变红。
TIMEOUT_TEXT = "应答超时".encode()
# plan modal 的标题（`ui/v2/modal.rs` 的 `📋 计划评审 · {review_type}`）。
PLAN_TITLE = "计划评审".encode()
# 否定路径的后端权威证据：判据全部取自 `GET /sessions/{seed}/timeline` 的物化快照，
# 不看 TUI 字节流（「模态消失」只证明面板下架，不证明裁决送达后端）。
# 出处（后端锚点 `1e78b7ce` = `tui-anchor-2026-09-23-v2.0.0-rc`）：
#   permission-deny → 工具终态 failed + 输出 `[DENIED] '…' (user denied permission)`
#   ask-dismiss     → `engine_turn.rs::handle_ask_dismiss`：回合落 `TimelineTurnState::Cancelled`
#   plan-reject     → `engine_turn.rs::handle_plan_response`：工具结果 `Plan rejected: <理由>`
REJECT_REASON = "e2e-plan-reject-marker"
deadline = start + 45

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
    if base == "permission":
        if b"\xe6\x89\xb9\xe5\x87\x86" in capture and not resolved:
            key = b"d" if mode == "permission-deny" else b"a"
            os.write(master, key)
            resolved = True
            resolved_at = time.monotonic()
            print(
                "permission modal detected; denied with d"
                if key == b"d"
                else "permission modal detected; approved with a"
            )
    elif base == "ask":
        if b"Continue with the test?" in capture and not resolved:
            key = b"\x1b" if mode == "ask-dismiss" else b"1"
            os.write(master, key)
            resolved = True
            resolved_at = time.monotonic()
            print(
                "ask modal detected; dismissed with Esc"
                if key == b"\x1b"
                else "ask modal detected; answered with 1"
            )
    elif base == "plan":
        if PLAN_TITLE in capture and not resolved:
            if mode == "plan-reject":
                # 两段式：`r` 只进理由输入态，`Enter` 才真正提交裁决。
                os.write(master, b"r")
                resolved = True
                reject_entered_at = time.monotonic()
                print("plan modal detected; entered reject-reason mode with r")
            else:
                os.write(master, b"a")
                resolved = True
                resolved_at = time.monotonic()
                print("plan modal detected; approved with a")
        if (
            mode == "plan-reject"
            and reject_entered_at is not None
            and not sent_reject_reason
            and time.monotonic() - reject_entered_at >= 1.0
        ):
            os.write(master, REJECT_REASON.encode() + b"\r")
            sent_reject_reason = True
            resolved_at = time.monotonic()
            print("reject reason submitted with Enter")
    elif base == "pager":
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
    if is_hang:
        # hang 钩子让应答命令**永不返回 ack**，且该请求会永久占住连接任务
        # （后端 spec §1.2 明写：测试结束必须终止 daemon，不能等它自己返回）。
        # 所以：先断言到超时提示 → 杀 daemon → 再退出，验证 TUI 在 daemon
        # 消失后仍能干净退出。
        if not daemon_killed and TIMEOUT_TEXT in capture:
            os.killpg(daemon.pid, signal.SIGKILL)
            daemon.wait(timeout=5)
            daemon_killed = True
            print("timeout toast observed; daemon killed")
        if daemon_killed and not quit_sent:
            os.write(master, b"\x11")
            quit_sent = True
            print("Ctrl+Q sent after daemon killed")
    elif resolved_at is not None and not quit_sent and time.monotonic() - resolved_at >= 2.0:
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

# ── 否定路径的后端权威证据 ──────────────────────────────────────────────
# TUI 侧「模态消失」只说明它把面板下架了；拒绝/跳过/驳回是否**真的落到后端**，
# 必须查后端物化 timeline。必须在杀 daemon 之前查。
def timeline_tools(page):
    """展平快照里的 tool block → [(turn, block)]（block 里带 tool 字典）。"""
    out = []
    for turn in page.get("snapshot", {}).get("turns", []):
        for rnd in turn.get("rounds", []):
            for block in rnd.get("blocks", []):
                if isinstance(block.get("tool"), dict):
                    out.append((turn, block))
    return out

def tool_text(tool):
    return f"{tool.get('summary', '')} {tool.get('output', '')}"

def decision_ref(decision):
    """后端 `record_interaction_resolution` 写进账本的 decision_ref 算法。

    后端：`serde_json::to_vec(&json!({"decision": decision}))` 再 sha256
    （`qaqh-runtime/src/agent/engine_turn.rs`）。这里复刻成**可校验的指纹**，
    断言时比子串匹配硬：账本里必须有一条 decision_ref 正好等于它。
    """
    body = json.dumps({"decision": decision}, separators=(",", ":")).encode()
    return "sha256:" + hashlib.sha256(body).hexdigest()

def ledger_decisions():
    """读后端会话事实账本（`sessions/*/events.jsonl`）里的 interaction_resolved。

    这是 daemon 自己 fsync 的持久化事实，不是 TUI 侧的自述；用它做否定路径的
    权威证据。返回 decision_ref 列表。
    """
    refs = []
    for path in (DATA / "sessions").glob("*/events.jsonl"):
        for line in path.read_text(errors="replace").splitlines():
            line = line.strip()
            if not line:
                continue
            try:
                fact = json.loads(line)
            except json.JSONDecodeError:
                continue
            payload = fact.get("payload") or {}
            if payload.get("kind") != "interaction_resolved":
                continue
            refs.append((payload.get("data") or {}).get("decision_ref"))
    return refs

def negative_evidence(page, decisions):
    """返回 [(名称, 是否通过, 细节)]。判据全部来自后端侧，不看 TUI 字节流。"""
    turns = page.get("snapshot", {}).get("turns", [])
    states = [t.get("state") for t in turns]
    tools = timeline_tools(page)
    if mode == "permission-deny":
        hit = [
            block["tool"]
            for _, block in tools
            if block["tool"].get("state") == "failed"
            and "[DENIED]" in tool_text(block["tool"])
        ]
        return [
            (
                f"timeline：tool 终态 failed + [DENIED]（turns={states}）",
                bool(hit),
                "",
            )
        ]
    if mode == "ask-dismiss":
        ok = "cancelled" in states and decision_ref("dismissed") in decisions
        return [
            (
                f"timeline 回合 cancelled + 账本 decision=dismissed（turns={states}）",
                ok,
                "",
            )
        ]
    # plan-reject：三重判据（后端 #301 收口了钩子保真度，见 TUI issue #44）。
    # ① timeline 物化：块存在 + name/state/output + 回合 rounds 非空；
    # ② provider 上下文：assistant(tool_calls) 与 role=tool 配对、理由回灌；
    # ③ 会话事实账本：decision=rejected 指纹（保留为第三重）。
    out = []
    plan_blocks = [
        (turn, block)
        for turn, block in tools
        if str(block.get("block_id", "")).startswith("tool:test-plan-review-")
    ]
    out.append(
        (
            "timeline：存在 tool:test-plan-review-* 块",
            bool(plan_blocks),
            f"n={len(plan_blocks)}",
        )
    )
    tool = plan_blocks[0][1].get("tool", {}) if plan_blocks else {}
    output = str(tool.get("output") or "")
    out.append(
        (
            "timeline：tool.name == plan_submit",
            tool.get("name") == "plan_submit",
            f"name={tool.get('name')!r}",
        )
    )
    out.append(
        (
            "timeline：tool.state == failed",
            tool.get("state") == "failed",
            f"state={tool.get('state')!r}",
        )
    )
    out.append(
        (
            "timeline：tool.output 含 `Plan rejected: <理由>`",
            "Plan rejected:" in output and REJECT_REASON in output,
            repr(output[:120]),
        )
    )
    rounds = len(plan_blocks[0][0].get("rounds", [])) if plan_blocks else 0
    out.append(
        (
            "timeline：所属回合 rounds 非空",
            rounds > 0,
            f"rounds={rounds}（钩子不物化块时这里是 0）",
        )
    )

    calls = {}
    tool_msgs = []
    for request in server.requests:
        for message in request.get("messages", []):
            if message.get("role") == "assistant":
                for call in message.get("tool_calls") or []:
                    calls[call.get("id")] = (call.get("function") or {}).get("name")
            if message.get("role") == "tool":
                tool_msgs.append(
                    (message.get("tool_call_id"), str(message.get("content") or ""))
                )
    plan_calls = {
        cid: name for cid, name in calls.items() if str(cid).startswith("test-plan-review-")
    }
    out.append(
        (
            "provider：assistant tool_calls 含 test-plan-review-*",
            bool(plan_calls),
            f"calls={plan_calls}",
        )
    )
    paired = [(cid, content) for cid, content in tool_msgs if cid in plan_calls]
    out.append(
        (
            "provider：存在同 id 的 role=tool 消息",
            bool(paired),
            f"tool_call_ids={[cid for cid, _ in tool_msgs]}",
        )
    )
    out.append(
        (
            "provider：tool 内容含拒绝理由（理由回灌模型上下文）",
            any(REJECT_REASON in content for _, content in paired),
            repr(paired[0][1][:120]) if paired else "",
        )
    )
    out.append(
        (
            "账本：decision=rejected 指纹（第三重）",
            decision_ref("rejected") in decisions,
            "",
        )
    )
    return out

timeline_text = ""
negative_checks = []
if is_negative:
    deadline = time.monotonic() + 15
    while time.monotonic() < deadline:
        try:
            page = get_json(
                f"{endpoint}/ringing/v1/sessions/{created_seed}/timeline",
                {
                    "Authorization": f"Bearer {token}",
                    "X-QAQH-Client-Session-Id": client_session_id,
                },
            )
            timeline_text = json.dumps(page, ensure_ascii=False)
            negative_checks = negative_evidence(page, ledger_decisions())
        except Exception as exc:  # noqa: BLE001 — 失败原因要进产物
            timeline_text = f"<timeline fetch failed: {type(exc).__name__}: {exc}>"
            negative_checks = [(f"后端证据查询失败（{exc}）", False, "")]
        if all(ok for _, ok, _ in negative_checks):
            break
        time.sleep(0.5)
    (D / "timeline.json").write_text(timeline_text)
    # 诊断产物：provider 每次请求体（plan-reject 的上下文判据来自这里）。
    (D / "provider-requests.json").write_text(
        json.dumps(server.requests, ensure_ascii=False, indent=2)
    )

if daemon.poll() is None:
    os.killpg(daemon.pid, signal.SIGKILL)
    daemon.wait(timeout=5)
server.shutdown()

RAW.write_bytes(capture)
print(f"tui exit={tui.returncode} bytes={len(capture)} cursor_queries={queries}")

raw = bytes(capture)
if base == "permission":
    checks = [
        ("permission modal visible", b"\xe6\x89\xb9\xe5\x87\x86" in raw),
        (
            "permission denied (d)" if mode == "permission-deny" else "permission approved (a)",
            resolved,
        ),
    ]
elif base == "ask":
    checks = [
        ("ask modal visible", b"Continue with the test?" in raw),
        (
            "ask dismissed (Esc)" if mode == "ask-dismiss" else "ask answered (1)",
            resolved,
        ),
    ]
elif base == "plan":
    checks = [
        ("plan modal visible", PLAN_TITLE in raw),
        (
            "plan rejected (r+理由+Enter)"
            if mode == "plan-reject"
            else "plan approved (a)",
            resolved and (sent_reject_reason or mode != "plan-reject"),
        ),
    ]
else:
    checks = [
        ("pager body visible", b"pager reasoning body" in raw),
        ("pager returned", quit_sent),
    ]
if is_negative:
    # 本组模式的**真正判据**：后端权威证据（timeline 物化 / provider 上下文 /
    # 会话事实账本）。每条独立成行，红了能直接看出是哪一层缺。
    for name, passed, detail in negative_checks:
        suffix = f"  {detail}" if detail and not passed else ""
        checks += [(f"后端证据 · {name}{suffix}", passed)]
if is_hang:
    # 这三条才是 `*-hang` 模式的**目的**：不是「没崩」，而是
    # ① 应答超时真的变成用户可见的终态；② daemon 被终止后 TUI 能干净退出。
    checks += [
        ("应答超时提示可见", TIMEOUT_TEXT in raw),
        ("超时后 daemon 已被回收", daemon_killed),
        ("daemon 消失后仍能干净退出", quit_sent and tui.returncode == 0),
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
