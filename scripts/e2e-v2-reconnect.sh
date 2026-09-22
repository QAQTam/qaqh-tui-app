#!/bin/bash
# 真机端到端：V2 Agent View 断线重连（M6.3）。
#
# 判据：
#   ① daemon#1 被 kill 后 TUI 进入 lost；
#   ② daemon#2 以同一 data root 启动后 TUI 恢复 ready；
#   ③ 全程无 cursor-position timeout / panic，退出码为 0。
#
# 模式：
#   RECONNECT_MODE=auto   （默认）客户端后台自动恢复
#   RECONNECT_MODE=manual 在 lost 后按 Ctrl+R，覆盖手动重连入口
#
# 用法：
#   scripts/e2e-v2-reconnect.sh
#   RECONNECT_MODE=manual scripts/e2e-v2-reconnect.sh
# 前置：qaqh-daemon 与 qaqh-tui 已构建。
# 隔离：私有 QAQH_DATA_DIR（/tmp 下），不碰正在运行的 daemon 与会话。
# 注意：不要用 `pkill -f` 清理；本脚本全程按显式 PID 操作。

set -u

SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
REPO_ROOT=$(cd -- "$SCRIPT_DIR/.." && pwd)
BACKEND_ROOT=${QAQH_BACKEND_ROOT:-$REPO_ROOT/../qaqh-backend}
DAEMON=${DAEMON:-$BACKEND_ROOT/target/debug/qaqh-daemon}
TUI=${TUI:-$REPO_ROOT/target/debug/qaqh-tui}
D=${D:-/tmp/qaqh-e2e-v2-reconnect}

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
mkdir -p "$D/qaqh/sessions/aaaa1111"
cat >"$D/qaqh/sessions/aaaa1111/meta.json" <<'JSON'
{"seed":"aaaa1111","created_at":1757900000000,"updated_at":1757900000000,"model":"m1",
 "message_count":0,"turn_count":0,"mode":0,"archived":false,"ephemeral":false,
 "title":"Reconnect Probe"}
JSON

QAQH_DATA_DIR="$D/qaqh" DAEMON="$DAEMON" TUI="$TUI" D="$D" \
    RECONNECT_MODE="${RECONNECT_MODE:-auto}" python3 - <<'PY'
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
import time

D = pathlib.Path(os.environ["D"])
DAEMON = os.environ["DAEMON"]
TUI = os.environ["TUI"]
DATA = D / "qaqh"
DISCOVERY = DATA / "daemon.json"
DAEMON_LOG = D / "daemon.out"
RAW = D / "tui.raw"
mode = os.environ.get("RECONNECT_MODE", "auto")
if mode not in {"auto", "manual"}:
    raise SystemExit(f"RECONNECT_MODE must be auto|manual, got {mode!r}")

def start_daemon():
    log = DAEMON_LOG.open("ab")
    env = os.environ.copy()
    env["QAQH_DATA_DIR"] = str(DATA)
    proc = subprocess.Popen(
        [DAEMON, "run"],
        stdin=subprocess.DEVNULL,
        stdout=log,
        stderr=subprocess.STDOUT,
        env=env,
        start_new_session=True,
    )
    deadline = time.monotonic() + 20
    while time.monotonic() < deadline:
        try:
            payload = json.loads(DISCOVERY.read_text())
        except (OSError, json.JSONDecodeError):
            time.sleep(0.2)
            continue
        if payload.get("pid") == proc.pid:
            return proc, payload
        time.sleep(0.2)
    proc.kill()
    raise RuntimeError("daemon did not publish daemon.json")

def stop_daemon(proc):
    if proc.poll() is None:
        os.killpg(proc.pid, signal.SIGKILL)
        proc.wait(timeout=5)
    DISCOVERY.unlink(missing_ok=True)

daemon1, discovery1 = start_daemon()
print(f"daemon#1 pid={daemon1.pid} epoch={discovery1.get('server_epoch')}")

master, slave = pty.openpty()
fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 40, 130, 0, 0))
env = os.environ.copy()
env["TERM"] = "xterm-256color"
env["QAQH_DATA_DIR"] = str(DATA)
tui = subprocess.Popen(
    [TUI, "--v2-agent", "--no-spawn"],
    stdin=slave,
    stdout=slave,
    stderr=slave,
    env=env,
    start_new_session=True,
    close_fds=True,
)
os.close(slave)
os.set_blocking(master, False)

capture = bytearray()
query_tail = bytearray()
start = time.monotonic()
queries = 0
killed = False
daemon2 = None
restarted = False
quit_sent = False
next_key = 0
key_schedule = [
    (3.0, b"\x0c"),  # Ctrl+L：Sessions
    (5.0, b"\r"),    # 打开隔离会话，让 status line 出现
]
if mode == "manual":
    key_schedule.extend(
        [
            (30.0, b"\x12"),  # Ctrl+R：daemon 仍 dead，覆盖失败/重试提示
            (36.0, b"\x12"),  # Ctrl+R：daemon#2 已启动
        ]
    )
    restart_at = 33.0
    quit_at = 56.0
    deadline = start + 70
else:
    restart_at = 30.0
    quit_at = 52.0
    deadline = start + 65

print(f"mode={mode}")

while time.monotonic() < deadline:
    now = time.monotonic()
    elapsed = now - start

    while next_key < len(key_schedule) and elapsed >= key_schedule[next_key][0]:
        os.write(master, key_schedule[next_key][1])
        next_key += 1

    if not killed and elapsed >= 7.0:
        stop_daemon(daemon1)
        killed = True
        print("daemon#1 killed at t=7")

    if killed and not restarted and elapsed >= restart_at:
        daemon2, discovery2 = start_daemon()
        restarted = True
        print(
            f"daemon#2 pid={daemon2.pid} epoch={discovery2.get('server_epoch')} "
            f"started at t={restart_at:.0f}"
        )

    if restarted and not quit_sent and elapsed >= quit_at:
        os.write(master, b"\x11")
        quit_sent = True
        print(f"Ctrl+Q sent at t={quit_at:.0f}")

    ready, _, _ = select.select([master], [], [], 0.05)
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

if daemon2 is not None:
    stop_daemon(daemon2)
else:
    stop_daemon(daemon1)

RAW.write_bytes(capture)
print(f"tui exit={tui.returncode} bytes={len(capture)} cursor_queries={queries}")

raw = bytes(capture)
checks = [
    ("无 cursor-position timeout", b"cursor position could not be read" not in raw),
    ("观察到 ready", b"ready" in raw),
    ("观察到 lost", b"lost" in raw),
    (
        "最终恢复到 ready",
        raw.rfind(b"ready") > raw.rfind(b"lost"),
    ),
    ("无 panic", b"panicked at" not in raw),
]
if mode == "manual":
    manual_markers = [
        "正在重连 daemon".encode(),
        "重连失败".encode(),
        "连接正常，无需重连".encode(),
    ]
    checks.append(("手动重连动作可见", any(marker in raw for marker in manual_markers)))
ok = tui.returncode == 0
for name, passed in checks:
    ok &= passed
    print(f"  [{'✓' if passed else '✗'}] {name}")
print("RESULT:", "PASS" if ok else "FAIL")
raise SystemExit(0 if ok else 1)
PY
