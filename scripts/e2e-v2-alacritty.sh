#!/bin/bash
# 真机端到端：V2 Agent View 在 **Alacritty** 里的链路（M6.3 / 兼容矩阵 §3）。
#
# ⚠️ 本脚本是**部分覆盖**，不声称渲染通过 —— 理由必须写清楚：
#
#   Alacritty **没有 IPC**（只有 `alacritty msg` 的窗口/配置子命令，没有任何
#   屏幕回读、也没有按键注入）。所以 kitty/tmux 那套「读回模拟器解出来的屏幕」
#   在这里做不到。本脚本只断言**进程级 + 环境级**事实：
#
#     1. TUI 真的跑在 Alacritty 的 pty 里（TERM=alacritty、真 /dev/pts、
#        真实行列数、terminfo 能解析）；
#     2. TUI 初始化到「连上 daemon」并持续存活（没崩）；
#     3. 日志里无 panic。
#
#   渲染正确性 / scrollback / resize / Ctrl+Q 退出**不在**本脚本能力内，
#   需要人工视觉确认。矩阵里 Alacritty 行因此标「部分」而不是 PASS——
#   拿进程级证据冒充渲染证据就是假绿。
#
# 用法：scripts/e2e-v2-alacritty.sh
# 前置：qaqh-daemon 与 qaqh-tui 已构建；`alacritty` 在 PATH 上。
# 缺 alacritty / 无可用显示环境时**显式 SKIP**（exit 0）。

set -u

SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
REPO_ROOT=$(cd -- "$SCRIPT_DIR/.." && pwd)
BACKEND_ROOT=${QAQH_BACKEND_ROOT:-$REPO_ROOT/../qaqh-backend}
DAEMON=${DAEMON:-$BACKEND_ROOT/target/debug/qaqh-daemon}
TUI=${TUI:-$REPO_ROOT/target/debug/qaqh-tui}
D=${D:-/tmp/qaqh-e2e-v2-alacritty}

for binary in "$DAEMON" "$TUI"; do
    [ -x "$binary" ] || {
        echo "缺少可执行文件：$binary（先 cargo build）" >&2
        exit 1
    }
done

if ! command -v alacritty >/dev/null 2>&1; then
    echo "SKIP: 未安装 alacritty —— Alacritty 行需要真实终端模拟器"
    exit 0
fi

case "$D" in
    /tmp/*) ;;
    *)
        echo "D 必须位于 /tmp 下：$D" >&2
        exit 1
        ;;
esac

python3 - "$D" <<'PY'
import pathlib
import shutil
import sys

d = pathlib.Path(sys.argv[1])
if d.exists():
    shutil.rmtree(d)
(d / "qaqh").mkdir(parents=True)
PY

setsid env QAQH_DATA_DIR="$D/qaqh" "$DAEMON" run </dev/null >>"$D/daemon.out" 2>&1 &
DAEMON_PID=$!
for _ in $(seq 1 40); do
    if grep -q "\"pid\": $DAEMON_PID" "$D/qaqh/daemon.json" 2>/dev/null; then
        break
    fi
    sleep 1
done
if ! grep -q "\"pid\": $DAEMON_PID" "$D/qaqh/daemon.json" 2>/dev/null; then
    echo "daemon 未起来；日志：" >&2
    cat "$D/daemon.out" >&2
    exit 1
fi
echo "daemon pid=$DAEMON_PID data=$D"

cleanup() {
    if [ -f "$D/ala-pid" ]; then
        kill "$(cat "$D/ala-pid")" 2>/dev/null || true
    fi
    kill "$DAEMON_PID" 2>/dev/null || true
    wait "$DAEMON_PID" 2>/dev/null || true
}
trap cleanup EXIT

D="$D" TUI="$TUI" python3 - <<'PY'
import json
import os
import pathlib
import subprocess
import time
import uuid
from urllib.request import Request, urlopen

D = pathlib.Path(os.environ["D"])
DATA = D / "qaqh"
TUI = os.environ["TUI"]
DISCOVERY = DATA / "daemon.json"

def post_json(url, payload, headers=None):
    body = json.dumps(payload).encode()
    request = Request(url, data=body, method="POST")
    request.add_header("Content-Type", "application/json")
    for key, value in (headers or {}).items():
        request.add_header(key, value)
    with urlopen(request, timeout=10) as response:
        return json.loads(response.read())

(DATA / "config.toml").write_text(
    'provider_id = "openai"\nactive_profile = "default"\npermission_level = 2\n\n'
    '[profiles.default]\nmodel = "fake-model"\nmax_tokens = 4096\neffort = "low"\n'
    'context_limit = 100000\nbase_url = "http://127.0.0.1:1/v1"\nendpoint = "openai"\n'
)

discovery = json.loads(DISCOVERY.read_text())
endpoint = discovery["endpoint"].rstrip("/")
token = discovery["token"]
client_instance_id = str(uuid.uuid4())
opened = post_json(
    f"{endpoint}/ringing/v1/clients/open",
    {"schema": "qaqh.Ringing", "version": 1, "client_instance_id": client_instance_id},
    {"Authorization": f"Bearer {token}"},
)
client_session_id = opened["client_session_id"]
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
seed = None
deadline = time.monotonic() + 20
while time.monotonic() < deadline and not seed:
    for meta in (DATA / "sessions").glob("*/meta.json"):
        try:
            seed = json.loads(meta.read_text()).get("seed")
        except (OSError, json.JSONDecodeError):
            continue
        if seed:
            break
    time.sleep(0.2)
if not seed:
    raise RuntimeError("session_create 没产出会话")
print(f"created seed={seed}")

# ── 在真实 Alacritty 里跑 TUI ───────────────────────────────────────────
# 环境事实（TERM / COLORTERM / pty / 行列数 / terminfo）由 wrapper 从
# **pane 内部**采集——这是 Alacritty 侧唯一可信的观测面。
#
# 连通性用 `qaqh-tui doctor` 钉：它跑的是 discovery → pid 判活 → /health →
# open 握手的**真链路**，输出 `[4] OK` 才算通。比在日志里 grep
# "starting new connection" 硬得多——后者只证明「发起过连接尝试」。
EVIDENCE = D / "evidence.txt"
DOCTOR = D / "doctor.txt"
TUILOG = D / "tui.log"
wrapper = D / "run-in-alacritty.sh"
wrapper.write_text(
    "#!/bin/bash\n"
    "unset NO_COLOR\n"
    f'LOG="{EVIDENCE}"\n'
    "{\n"
    '  echo "TERM=$TERM"\n'
    '  echo "COLORTERM=${COLORTERM:-<unset>}"\n'
    '  echo "TTY=$(tty 2>/dev/null)"\n'
    '  echo "STTY_SIZE=$(stty size 2>/dev/null)"\n'
    '  echo "TERMINFO_OK=$(infocmp "$TERM" >/dev/null 2>&1 && echo yes || echo no)"\n'
    "} > \"$LOG\"\n"
    f'export QAQH_DATA_DIR="{DATA}"\n'
    f'export QAQH_TUI_LOG="{TUILOG}"\n'
    # ① 真握手自检（在 Alacritty 的 pty 内跑）
    f'"{TUI}" doctor > "{DOCTOR}" 2>&1\n'
    'echo "DOCTOR_EXIT=$?" >> "$LOG"\n'
    # ② 真跑 TUI
    f'"{TUI}" --no-spawn\n'
    'echo "TUI_EXIT=$?" >> "$LOG"\n'
    "sleep 120\n"
)
wrapper.chmod(0o755)

ala = subprocess.Popen(
    [
        "alacritty",
        "-o",
        "window.dimensions.columns=100",
        "-o",
        "window.dimensions.lines=35",
        "-e",
        str(wrapper),
    ],
    stdin=subprocess.DEVNULL,
    stdout=subprocess.DEVNULL,
    stderr=subprocess.DEVNULL,
    start_new_session=True,
)
(D / "ala-pid").write_text(str(ala.pid))

checks = []
def check(name, passed, detail=""):
    checks.append((name, bool(passed), detail))
    return passed

def evidence():
    try:
        return EVIDENCE.read_text()
    except OSError:
        return ""

deadline = time.monotonic() + 25
while time.monotonic() < deadline and "TERM=" not in evidence():
    if ala.poll() is not None:
        break
    time.sleep(0.4)

ev = evidence()
if "TERM=" not in ev:
    print("SKIP: Alacritty 起不来（多半是没有可用显示环境）")
    raise SystemExit(0)

check("环境：跑在 Alacritty 的 pty 里（TERM=alacritty）", "TERM=alacritty" in ev, ev.strip())
check("环境：真 pty（/dev/pts/*）", "TTY=/dev/pts/" in ev, "")
check(
    "环境：Alacritty 的 terminfo 可解析（否则 crossterm 根本画不出来）",
    "TERMINFO_OK=yes" in ev,
    "",
)
check(
    "环境：真实行列数 = 请求的 100x35",
    "STTY_SIZE=35 100" in ev,
    ev.strip(),
)

# TUI 活着的证据：进程还在 + 日志已开始写 + 日志里无 panic。
def tui_alive():
    out = subprocess.run(
        ["pgrep", "-f", f"{TUI} --no-spawn"],
        capture_output=True,
        text=True,
    )
    return bool(out.stdout.strip())

time.sleep(6.0)
check("进程：TUI 在 Alacritty 里持续存活（未崩）", tui_alive(), "")
check("进程：Alacritty 自身仍存活", ala.poll() is None, f"rc={ala.poll()}")

log = TUILOG.read_text() if TUILOG.exists() else ""
(D / "tui.log").write_text(log)
check("日志：TUI 写出了诊断日志（说明真的初始化过）", len(log.strip()) > 0, f"bytes={len(log)}")
check("日志：无 panic", "panicked at" not in log, "")

# 连通性：`doctor` 的四步握手必须在 Alacritty 的 pty 内全部走通。
doctor = DOCTOR.read_text() if DOCTOR.exists() else ""
(D / "doctor.out").write_text(doctor)
check(
    "连通性：doctor 在 Alacritty 内走完 discovery→存活→/health→open 握手（[4] OK）",
    "[4] OK" in doctor,
    repr(doctor[-200:]),
)
check(
    "连通性：拿到 client session 与 lease",
    "open: session=" in doctor and "lease_ttl=" in doctor,
    repr(doctor[:200]),
)
check(
    "连通性：doctor 退出码为 0",
    "DOCTOR_EXIT=0" in evidence(),
    evidence().strip(),
)

ok = True
for name, passed, detail in checks:
    ok &= passed
    suffix = f"  {detail}" if (detail and not passed) else ""
    print(f"  [{'✓' if passed else '✗'}] {name}{suffix}")
print("NOTE: 渲染 / scrollback / resize / 退出 不在本脚本能力内（Alacritty 无 IPC），需人工确认")
print("RESULT:", "PASS(partial)" if ok else "FAIL")
raise SystemExit(0 if ok else 1)
PY
