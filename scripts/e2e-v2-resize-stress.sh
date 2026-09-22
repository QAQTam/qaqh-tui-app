#!/bin/bash
# 真机端到端：V2 Agent View 高频 resize 压力（M6.3）。
#
# 判据：
#   ① 连续高度变化触发多次 inline viewport 重建；
#   ② 无 cursor-position timeout / panic；
#   ③ TUI 正常退出。
#
# 用法：scripts/e2e-v2-resize-stress.sh
# 前置：qaqh-daemon 与 qaqh-tui 已构建。
# 隔离：私有 QAQH_DATA_DIR（/tmp 下），不碰正在运行的 daemon 与会话。
# 注意：不要用 `pkill -f` 清理；本脚本全程按显式 PID 操作。

set -u

SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
REPO_ROOT=$(cd -- "$SCRIPT_DIR/.." && pwd)
BACKEND_ROOT=${QAQH_BACKEND_ROOT:-$REPO_ROOT/../qaqh-backend}
DAEMON=${DAEMON:-$BACKEND_ROOT/target/debug/qaqh-daemon}
TUI=${TUI:-$REPO_ROOT/target/debug/qaqh-tui}
D=${D:-/tmp/qaqh-e2e-v2-resize-stress}

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
 "title":"Resize Stress"}
JSON

start_daemon() {
    QAQH_DATA_DIR="$D/qaqh" "$DAEMON" run </dev/null >>"$D/daemon.out" 2>&1 &
    echo $!
}

wait_for_daemon() {
    local pid=$1
    for _ in $(seq 1 40); do
        if grep -q "\"pid\": $pid" "$D/qaqh/daemon.json" 2>/dev/null; then
            return 0
        fi
        sleep 1
    done
    return 1
}

DAEMON_PID=$(start_daemon)
if ! wait_for_daemon "$DAEMON_PID"; then
    echo "daemon 未起来；日志：" >&2
    cat "$D/daemon.out" >&2
    exit 1
fi
echo "daemon pid=$DAEMON_PID data=$D"

QAQH_DATA_DIR="$D/qaqh" TUI="$TUI" OUT="$D/tui.raw" python3 - <<'PY'
import fcntl
import os
import pathlib
import pty
import select
import signal
import struct
import subprocess
import termios
import time

out_path = pathlib.Path(os.environ["OUT"])
master, slave = pty.openpty()
fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 40, 130, 0, 0))
env = os.environ.copy()
env["TERM"] = "xterm-256color"
proc = subprocess.Popen(
    [os.environ["TUI"], "--v2-agent", "--no-spawn"],
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
keys = [
    (3.0, b"\x0c"),  # Ctrl+L
    (5.0, b"\r"),    # 打开会话
]
next_key = 0
heights = [12, 16, 20, 24, 32, 40, 48, 36, 28, 18]
next_resize = 0
resize_started = False
quit_sent = False
deadline = start + 28

while time.monotonic() < deadline:
    now = time.monotonic()
    elapsed = now - start

    while next_key < len(keys) and elapsed >= keys[next_key][0]:
        os.write(master, keys[next_key][1])
        next_key += 1

    if elapsed >= 8.0 and not resize_started:
        resize_started = True

    if resize_started and next_resize < 80:
        expected = 8.0 + next_resize * 0.1
        if elapsed >= expected:
            height = heights[next_resize % len(heights)]
            fcntl.ioctl(
                master,
                termios.TIOCSWINSZ,
                struct.pack("HHHH", height, 130, 0, 0),
            )
            os.killpg(proc.pid, signal.SIGWINCH)
            next_resize += 1

    if next_resize >= 80 and not quit_sent and elapsed >= 19.0:
        os.write(master, b"\x11")
        quit_sent = True

    ready, _, _ = select.select([master], [], [], 0.02)
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

    if proc.poll() is not None:
        break

if proc.poll() is None:
    os.killpg(proc.pid, signal.SIGTERM)
    try:
        proc.wait(timeout=2)
    except subprocess.TimeoutExpired:
        os.killpg(proc.pid, signal.SIGKILL)

out_path.write_bytes(capture)
print(
    f"tui exit={proc.returncode} bytes={len(capture)} "
    f"resizes={next_resize} cursor_queries={queries}"
)

raw = bytes(capture)
checks = [
    ("80 次 resize 已注入", next_resize >= 80),
    ("触发多次 viewport 重建", queries >= 3),
    ("无 cursor-position timeout", b"cursor position could not be read" not in raw),
    ("无 panic", b"panicked at" not in raw),
]
ok = proc.returncode == 0
for name, passed in checks:
    ok &= passed
    print(f"  [{'✓' if passed else '✗'}] {name}")
print("RESULT:", "PASS" if ok else "FAIL")
raise SystemExit(0 if ok else 1)
PY

kill "$DAEMON_PID" 2>/dev/null || true
wait "$DAEMON_PID" 2>/dev/null || true
