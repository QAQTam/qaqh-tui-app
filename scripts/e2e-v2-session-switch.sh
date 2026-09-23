#!/bin/bash
# 真机端到端：V2 Agent View 的会话切换与 scrollback purge（M6.2/M6.3）。
#
# 为什么需要它：内存单测能证明 AgentState 请求 reset + replay，但证明不了
# alternate screen 退出时终端真的执行了 ESC[3J，也覆盖不到 crossterm EventStream
# 与 cursor-position 查询之间的真实竞争。
#
# 判据：
#   ① Workspace 进出与 resize 后 TUI 仍正常退出（无 cursor position timeout）；
#   ② 两个会话标题都曾渲染；
#   ③ raw PTY 输出至少出现 2 次 ESC[3J：首次打开会话 + A/B 切换；
#   ④ alternate screen 有进入/退出记录。
#
# 用法：scripts/e2e-v2-session-switch.sh
# 前置：qaqh-daemon 与 qaqh-tui 已构建。
# 隔离：私有 QAQH_DATA_DIR（/tmp 下），不碰正在运行的 daemon 与会话。
# 注意：不要用 `pkill -f` 清理；本脚本全程按显式 PID 操作。

set -u

SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
REPO_ROOT=$(cd -- "$SCRIPT_DIR/.." && pwd)
# 默认吃**锚点 worktree**（TUI 钉的 rev，见 scripts/ci-linux.sh 的 QAQH_BACKEND_REV），
# 而不是开发者正在用的 ../qaqh-backend 工作树——否则 e2e 会跑到别人分支构建的
# daemon 上，与本仓门禁的锚点不是同一个东西。
BACKEND_ROOT=${QAQH_BACKEND_ROOT:-$REPO_ROOT/../qaqh-backend-anchor}
DAEMON=${DAEMON:-$BACKEND_ROOT/target/debug/qaqh-daemon}
TUI=${TUI:-$REPO_ROOT/target/debug/qaqh-tui}
D=${D:-/tmp/qaqh-e2e-v2-session-switch}

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
mkdir -p "$D/qaqh/sessions"

put_session() {
    local seed=$1 title=$2
    mkdir -p "$D/qaqh/sessions/$seed"
    cat >"$D/qaqh/sessions/$seed/meta.json" <<JSON
{"seed":"$seed","created_at":1757900000000,"updated_at":1757900000000,"model":"m1",
 "message_count":0,"turn_count":0,"mode":0,"archived":false,"ephemeral":false,"title":"$title"}
JSON
}

put_session aaaa1111 "Session Alpha"
put_session bbbb2222 "Session Beta"

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
schedule = [
    (4.0, b"\x0c"),      # Ctrl+L：Sessions
    (6.0, b"\x1b"),      # Esc：返回 Agent View
    (8.0, b"\x0c"),      # Ctrl+L：再次打开 Sessions
    (10.0, b"\r"),       # 打开第一会话
    (14.0, b"\x0c"),     # Ctrl+L：再次打开 Sessions
    (16.0, b"\x1b[B"),   # Down：第二会话
    (17.0, b"\r"),       # 切换
    (20.0, b"\x11"),     # Ctrl+Q
]
next_key = 0
resized = False
deadline = start + 28

while time.monotonic() < deadline:
    now = time.monotonic()
    while next_key < len(schedule) and now - start >= schedule[next_key][0]:
        os.write(master, schedule[next_key][1])
        next_key += 1
    if not resized and now - start >= 12.0:
        fcntl.ioctl(
            master,
            termios.TIOCSWINSZ,
            struct.pack("HHHH", 32, 100, 0, 0),
        )
        os.killpg(proc.pid, signal.SIGWINCH)
        resized = True

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

    if proc.poll() is not None:
        break

if proc.poll() is None:
    os.killpg(proc.pid, signal.SIGTERM)
    try:
        proc.wait(timeout=2)
    except subprocess.TimeoutExpired:
        os.killpg(proc.pid, signal.SIGKILL)

out_path.write_bytes(capture)
print(f"tui exit={proc.returncode} bytes={len(capture)} cursor_queries={queries}")
PY

kill "$DAEMON_PID" 2>/dev/null || true
wait "$DAEMON_PID" 2>/dev/null || true

python3 - "$D/tui.raw" <<'PY'
import pathlib
import sys

raw = pathlib.Path(sys.argv[1]).read_bytes()
checks = [
    ("无 cursor-position timeout", b"cursor position could not be read" not in raw),
    ("两个会话标题均渲染", b"Alpha" in raw and b"Beta" in raw),
    ("至少两次 scrollback purge", raw.count(b"\x1b[3J") >= 2),
    ("alternate screen 进出", b"\x1b[?1049h" in raw and b"\x1b[?1049l" in raw),
    ("无 panic", b"panicked at" not in raw),
]
ok = True
for name, passed in checks:
    ok &= passed
    print(f"  [{'✓' if passed else '✗'}] {name}")
print(f"  purge_count={raw.count(b'\x1b[3J')}")
print("RESULT:", "PASS" if ok else "FAIL")
sys.exit(0 if ok else 1)
PY
