#!/bin/bash
# 构建后自检：TUI 二进制能不能起来、首帧有没有渲染、有没有 panic。
#
# 为什么需要它：首帧空屏、初始化 panic 这类问题，单测未必覆盖真实终端路径；
# 「构建出来的那个二进制能不能跑」只有真跑一次才知道。
#
# 判据（缺一即红）：
#   ① 输出里**没有 panic**；
#   ② **首帧确实画出了东西**（去 ANSI 后仍有非空文本）——防「起来了但空屏」；
#   ③ 跑满窗口后 `Ctrl+Q` 干净退出（returncode 0）。
#
# 用法：scripts/smoke-tui.sh
# 前置：`cargo build --bin qaqh-tui` 与本机可用的 `qaqh-daemon`（不构建、不改任何生产代码）。
# 环境：`TUI` / `DAEMON` / `QAQH_BACKEND_ROOT` / `RUN_SECS` / `TUI_ARGS` 可覆盖。
#
# 隔离：私有 data root（/tmp 下）+ 自己起的 daemon，不碰你正在用的 daemon 与会话。
# ⚠ Linux 下 data root 的 basename 必须是 `qaqh`（Windows 才是 `.qaqh`）。
# ⚠ 不要用 `pkill -f <模式>` 清理：脚本自身的命令行就含那些模式。全程按显式 PID 操作。
#
# **它不覆盖什么**（如实标注）：本脚本跑的是**空会话**，只回答
# 「这个二进制起得来吗」，不覆盖长会话滚动与交互路径。

set -u

SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
REPO_ROOT=$(cd -- "$SCRIPT_DIR/.." && pwd)
# 默认吃兄弟仓 ../qaqh-backend；调用方仍可用 QAQH_BACKEND_ROOT 覆盖。
BACKEND_ROOT=${QAQH_BACKEND_ROOT:-$REPO_ROOT/../qaqh-backend}
DAEMON=${DAEMON:-$BACKEND_ROOT/target/debug/qaqh-daemon}
TUI=${TUI:-$REPO_ROOT/target/debug/qaqh-tui}
D=${D:-/tmp/qaqh-smoke-tui}
RUN_SECS=${RUN_SECS:-8}

# ⚠ 守卫：`D` 可被环境覆盖，而下面要清理它。误设 `D=/`、`D=/tmp`
# 或指向工作目录都会变成一次破坏性删除。
case "$D" in
  /tmp/*|/var/tmp/*) ;;
  *) echo "拒绝：D 必须落在 /tmp 或 /var/tmp 下（当前：$D）——本脚本会对它 rm -rf" >&2; exit 1 ;;
esac

[ -x "$DAEMON" ] || { echo "缺少 daemon：$DAEMON（本脚本不构建，请先构建后端）"; exit 1; }
[ -x "$TUI" ] || { echo "缺少 TUI：$TUI（本脚本不构建，请先 cargo build --bin qaqh-tui）"; exit 1; }
command -v python3 >/dev/null 2>&1 || { echo "需要 python3（输出解析）"; exit 1; }

HOME_DIR=$D/home
DATA=$HOME_DIR/qaqh
rm -rf "$D"; mkdir -p "$DATA"

DAEMON_PID=""
cleanup() {
  [ -n "$DAEMON_PID" ] && kill -9 "$DAEMON_PID" 2>/dev/null
  return 0
}
trap cleanup EXIT

QAQH_DATA_DIR="$DATA" HOME="$HOME_DIR" USERPROFILE="$HOME_DIR" \
  "$DAEMON" run </dev/null >>"$D/daemon.out" 2>&1 &
DAEMON_PID=$!

DISCOVERY=$DATA/daemon.json
ok=0
for _ in $(seq 1 40); do
  # 必须连 pid 一起核：只判「文件存在」会读到上一轮遗留的 daemon.json。
  if grep -q "\"pid\": $DAEMON_PID" "$DISCOVERY" 2>/dev/null; then ok=1; break; fi
  sleep 1
done
if [ "$ok" != 1 ]; then
  echo "daemon 未在 40s 内写出 discovery（pid=$DAEMON_PID）"
  sed -n '1,30p' "$D/daemon.out"
  exit 1
fi
echo "daemon pid=$DAEMON_PID 就绪；跑 TUI ${RUN_SECS}s…"

# 真 PTY 驱动：V2 fullscreen 初始化时会发 `ESC[6n` 查询光标，
# 旧版 `script` 驱动无人应答，会把合法的终端能力探测误判成 panic。
# 这里复用 e2e 的做法，显式回 `ESC[1;1R`，跑满窗口后 Ctrl+Q 干净退出。
python3 - "$TUI" "$DATA" "$D/tui.raw" "$RUN_SECS" <<'PY'
import fcntl
import os
import pathlib
import pty
import re
import select
import shlex
import signal
import struct
import subprocess
import sys
import termios
import time

TUI = sys.argv[1]
DATA = sys.argv[2]
RAW = pathlib.Path(sys.argv[3])
RUN_SECS = float(sys.argv[4])

master, slave = pty.openpty()
fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 40, 130, 0, 0))
env = os.environ.copy()
env["TERM"] = "xterm-256color"
env["QAQH_DATA_DIR"] = DATA
tui = subprocess.Popen(
    [TUI, *shlex.split(os.environ.get("TUI_ARGS", ""))],
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
quit_sent = False
while time.monotonic() - start < RUN_SECS + 3:
    elapsed = time.monotonic() - start
    if not quit_sent and elapsed >= RUN_SECS:
        os.write(master, b"\x11")  # Ctrl+Q
        quit_sent = True
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
                os.write(master, b"\x1b[1;1R")
    if tui.poll() is not None:
        break

if tui.poll() is None:
    os.killpg(tui.pid, signal.SIGTERM)
    try:
        tui.wait(timeout=2)
    except subprocess.TimeoutExpired:
        os.killpg(tui.pid, signal.SIGKILL)
        tui.wait(timeout=2)
os.close(master)

RAW.write_bytes(capture)
raw = capture.decode(errors="replace")
s = re.sub(r"\x1b\[[0-9;?]*[a-zA-Z]", "", raw).replace("\r", "")
panic = "panicked" in s
text = [line for line in s.split("\n") if line.strip()]
first = text[0] if text else ""
# 真 TUI 标识：V2 fullscreen 首帧画 `AgentView`，空会话首帧画 QAQH 品牌页。
marker = "qaqh-tui" in s or "AgentView" in s or "QAQ-HARNESS" in s
startup_error = bool(re.match(r"\s*(Error|error|thread .*panicked)", first))
print(f"  去 ANSI 后非空行: {len(text)}")
print(f"  panic           : {'✗ 有' if panic else '✓ 无'}")
print(f"  真 TUI 标识     : {'✓ 有' if marker else '✗ 无'}")
print(f"  启动错误首行    : {'✗ 有' if startup_error else '✓ 无'}")
print(f"  退出状态        : {tui.returncode}（跑满 {RUN_SECS:g}s 后 Ctrl+Q 应为 0）")
if panic:
    index = s.find("panicked")
    print("  >>", s[max(0, index - 140):index + 240].replace("\n", " ⏎ "))
print(f"  首屏片段        : {first[:100] if first else '(空)'}")
ok = bool(text) and not panic and marker and not startup_error and tui.returncode == 0
print("  判定:", "✓ 首帧渲染且未 panic" if ok else "✗ 失败（原始材料见脚本打印的目录）")
sys.exit(0 if ok else 1)
PY
