#!/bin/bash
# 构建后自检：TUI 二进制能不能起来、首帧有没有渲染、有没有 panic。
#
# 为什么需要它：issue #33（P0）就是**首帧**问题——`top` 被算两遍（refresh 前后 total 不同），
# 窗口落在未渲染块上：debug 直接 panic，release 整个视口静默变空。这类问题
# **单测锁能抓**（见 `render/mod.rs` 的两条 P0 锁），但「构建出来的那个二进制能不能跑」
# 只有真跑一次才知道。本脚本比 `e2e-lease-expiry.sh` 轻得多，适合每次构建后顺手跑。
#
# 判据（两条，缺一即红）：
#   ① 输出里**没有 panic**；
#   ② **首帧确实画出了东西**（去 ANSI 后仍有非空文本）——防「起来了但空屏」。
#
# 用法：scripts/smoke-tui.sh
# 前置：`cargo build --bin qaqh-tui` 与本机可用的 `qaqh-daemon`（不构建、不改任何生产代码）。
# 环境：`TUI` / `DAEMON` / `QAQH_BACKEND_ROOT` / `RUN_SECS` 可覆盖。
#
# 隔离：私有 data root（/tmp 下）+ 自己起的 daemon，不碰你正在用的 daemon 与会话。
# ⚠ Linux 下 data root 的 basename 必须是 `qaqh`（Windows 才是 `.qaqh`）。
# ⚠ 不要用 `pkill -f <模式>` 清理：脚本自身的命令行就含那些模式。全程按显式 PID 操作。
#
# **它不覆盖什么**（如实标注）：本脚本跑的是**空会话**（隔离 daemon 里没有历史），
# 所以走不到「长会话 → 视口落在未渲染块」那条路径。那条路径由 `render/mod.rs` 里的
# 夹具锁（`sweep_fixture(120)`）覆盖；本脚本只回答「这个二进制起得来吗」。

set -u

SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
REPO_ROOT=$(cd -- "$SCRIPT_DIR/.." && pwd)
BACKEND_ROOT=${QAQH_BACKEND_ROOT:-$REPO_ROOT/../qaqh-backend}
DAEMON=${DAEMON:-$BACKEND_ROOT/target/debug/qaqh-daemon}
TUI=${TUI:-$REPO_ROOT/target/debug/qaqh-tui}
D=${D:-/tmp/qaqh-smoke-tui}
RUN_SECS=${RUN_SECS:-8}

[ -x "$DAEMON" ] || { echo "缺少 daemon：$DAEMON（本脚本不构建，请先构建后端）"; exit 1; }
[ -x "$TUI" ] || { echo "缺少 TUI：$TUI（本脚本不构建，请先 cargo build --bin qaqh-tui）"; exit 1; }
command -v python3 >/dev/null 2>&1 || { echo "需要 python3（输出解析）"; exit 1; }

HOME_DIR=$D/home
DATA=$HOME_DIR/qaqh
rm -rf "$D"; mkdir -p "$DATA"

DAEMON_PID=""; FEED_PID=""
cleanup() {
  [ -n "$DAEMON_PID" ] && kill -9 "$DAEMON_PID" 2>/dev/null
  [ -n "$FEED_PID" ] && kill "$FEED_PID" 2>/dev/null
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

# pty + stdin 用 FIFO 持住（否则 EventStream 立刻 EOF，TUI 起来就退）。
FIFO=$D/in
mkfifo "$FIFO"
( exec 3>"$FIFO"; sleep "$RUN_SECS" ) & FEED_PID=$!

QAQH_DATA_DIR="$DATA" HOME="$HOME_DIR" USERPROFILE="$HOME_DIR" \
  timeout $((RUN_SECS + 12)) script -qec "stty rows 40 cols 130; timeout $RUN_SECS $TUI" /dev/null \
  <"$FIFO" >"$D/tui.raw" 2>&1
wait "$FEED_PID" 2>/dev/null
FEED_PID=""

python3 - "$D/tui.raw" <<'PY'
import re, sys, pathlib
raw = pathlib.Path(sys.argv[1]).read_text(errors="replace")
s = re.sub(r"\x1b\[[0-9;?]*[a-zA-Z]", "", raw).replace("\r", "")
panic = "panicked" in s
text = [l for l in s.split("\n") if l.strip()]
print(f"  去 ANSI 后非空行: {len(text)}")
print(f"  panic           : {'✗ 有' if panic else '✓ 无'}")
if panic:
    i = s.find("panicked")
    print("  >>", s[max(0, i - 140):i + 240].replace("\n", " ⏎ "))
print(f"  首屏片段        : {(text[0][:100] if text else '(空)')}")
ok = bool(text) and not panic
print("  判定:", "✓ 首帧渲染且未 panic" if ok else "✗ 失败（原始材料见脚本打印的目录）")
sys.exit(0 if ok else 1)
PY
