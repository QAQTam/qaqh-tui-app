#!/bin/bash
# 真机端到端：V2 Agent View 终端能力/主题降级矩阵（M6.3/M6.4 基线）。
#
# 说明：这里验证的是环境能力组合，不替代真实终端模拟器矩阵。
#
# 判据：每个 profile 都正常退出、无 panic、无 cursor-position timeout。
#
# 用法：scripts/e2e-v2-terminal-matrix.sh
# 前置：qaqh-daemon 与 qaqh-tui 已构建。
# 隔离：私有 QAQH_DATA_DIR（/tmp 下），不碰正在运行的 daemon 与会话。

set -u

SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
REPO_ROOT=$(cd -- "$SCRIPT_DIR/.." && pwd)
# 默认吃**锚点 worktree**（TUI 钉的 rev，见 scripts/ci-linux.sh 的 QAQH_BACKEND_REV），
# 而不是开发者正在用的 ../qaqh-backend 工作树——否则 e2e 会跑到别人分支构建的
# daemon 上，与本仓门禁的锚点不是同一个东西。
BACKEND_ROOT=${QAQH_BACKEND_ROOT:-$REPO_ROOT/../qaqh-backend}
DAEMON=${DAEMON:-$BACKEND_ROOT/target/debug/qaqh-daemon}
TUI=${TUI:-$REPO_ROOT/target/debug/qaqh-tui}
D=${D:-/tmp/qaqh-e2e-v2-terminal-matrix}

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
 "title":"Terminal Matrix"}
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

QAQH_DATA_DIR="$D/qaqh" TUI="$TUI" OUT_DIR="$D" python3 - <<'PY'
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

out_dir = pathlib.Path(os.environ["OUT_DIR"])
base_env = os.environ.copy()
base_env["QAQH_DATA_DIR"] = os.environ["QAQH_DATA_DIR"]

profiles = [
    ("night-16", {"TERM": "xterm", "QAQH_THEME": "night"}),
    ("night-256", {"TERM": "xterm-256color", "QAQH_THEME": "night"}),
    (
        "night-truecolor",
        {"TERM": "xterm-256color", "COLORTERM": "truecolor", "QAQH_THEME": "night"},
    ),
    ("day-256", {"TERM": "xterm-256color", "QAQH_THEME": "day"}),
    ("terminal-16", {"TERM": "xterm", "QAQH_THEME": "terminal"}),
    ("no-color", {"TERM": "xterm-256color", "NO_COLOR": "1"}),
    ("term-dumb", {"TERM": "dumb"}),
    ("tmux-256", {"TERM": "tmux-256color", "TMUX": "/tmp/tmux-fake,0,0"}),
    ("screen-256", {"TERM": "screen-256color"}),
    ("kitty", {"TERM": "xterm-kitty", "COLORTERM": "truecolor"}),
    ("alacritty", {"TERM": "alacritty", "COLORTERM": "truecolor"}),
    ("ssh-xterm", {"TERM": "xterm-256color", "SSH_TTY": "/dev/pts/0"}),
]

def run_profile(name, extra_env):
    env = base_env.copy()
    env.update(extra_env)
    for key in ["COLORTERM", "NO_COLOR", "QAQH_THEME", "TMUX", "SSH_TTY"]:
        if key not in extra_env:
            env.pop(key, None)

    master, slave = pty.openpty()
    fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 36, 120, 0, 0))
    proc = subprocess.Popen(
        [os.environ["TUI"], "--no-spawn"],
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
        (1.5, b"\x0c"),  # Ctrl+L
        (2.5, b"\r"),    # 打开会话
        (4.5, b"\x11"),  # Ctrl+Q
    ]
    next_key = 0
    deadline = start + 8

    while time.monotonic() < deadline:
        elapsed = time.monotonic() - start
        while next_key < len(keys) and elapsed >= keys[next_key][0]:
            os.write(master, keys[next_key][1])
            next_key += 1

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

        if proc.poll() is not None:
            break

    if proc.poll() is None:
        os.killpg(proc.pid, signal.SIGTERM)
        try:
            proc.wait(timeout=2)
        except subprocess.TimeoutExpired:
            os.killpg(proc.pid, signal.SIGKILL)

    raw = bytes(capture)
    (out_dir / f"{name}.raw").write_bytes(raw)
    # 鼠标/复制维度：v2 Agent View **不得**开启鼠标追踪——开了就吃掉终端原生
    # 选择/复制（README「该模式不启用鼠标捕获」的承诺）。这里直接扫原始字节流，
    # 因为这是「输出里有没有那几个私有模式序列」的问题，模拟器侧看不出来。
    mouse_enable = [
        seq
        for seq in (
            b"\x1b[?1000h",  # 基础按键上报
            b"\x1b[?1002h",  # 按键拖动
            b"\x1b[?1003h",  # 任意移动
            b"\x1b[?1006h",  # SGR 扩展坐标
            b"\x1b[?1015h",  # urxvt 扩展坐标
        )
        if seq in raw
    ]
    passed = (
        proc.returncode == 0
        and b"panicked at" not in raw
        and b"cursor position could not be read" not in raw
        and not mouse_enable
    )
    print(
        f"  [{'✓' if passed else '✗'}] {name:<16} "
        f"exit={proc.returncode} queries={queries} bytes={len(raw)} "
        f"mouse={'ON ' + ','.join(s.decode() for s in mouse_enable) if mouse_enable else 'off'}"
    )
    return passed

ok = True
for name, extra_env in profiles:
    ok &= run_profile(name, extra_env)

print("RESULT:", "PASS" if ok else "FAIL")
raise SystemExit(0 if ok else 1)
PY

kill "$DAEMON_PID" 2>/dev/null || true
wait "$DAEMON_PID" 2>/dev/null || true
