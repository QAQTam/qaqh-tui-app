#!/bin/bash
# 真机端到端：daemon 重启自愈 + Ctrl+R 重连（T-03）。
#
# 为什么需要它：这两条路径**只有真跑才有意义**——单测覆盖不了「daemon 换端口/
# token/epoch 后客户端能否自愈」。此前它们被后端 BUG-2026-09-15-02（daemon
# 启动期工具探测可无限挂起）堵住，2026-09-15 修复（后端 `674742f`）后本脚本
# 才跑得通。
#
# 用法：scripts/e2e-restart.sh
# 前置：两仓均已 `cargo build`。全程用**隔离 data root**（QAQH_DATA_DIR 指向
#       /tmp 下的私有目录），不碰你正在用的 daemon 与会话。
#
# 不要用 `pkill -f <模式>` 清理本脚本的进程：本脚本的命令行本身就含那些模式，
# pkill 会把自己的 shell 一起杀掉（实测 exit 144）。全程按显式 PID 操作。

set -u
DAEMON=${DAEMON:-$HOME/Projects/qaqh-backend/target/debug/qaqh-daemon}
TUI=${TUI:-$HOME/Projects/qaqh-tui-app/target/debug/qaqh-tui}
D=/tmp/qaqh-e2e-restart
PROBE_DELAY=${PROBE_DELAY:-70}   # 杀 daemon 后等多久按 Ctrl+R（须 > STALL_AFTER=60s）

for b in "$DAEMON" "$TUI"; do
  [ -x "$b" ] || { echo "缺少可执行文件：$b（先 cargo build）"; exit 1; }
done

rm -rf "$D"; mkdir -p "$D/qaqh"
FIFO=$D/in; mkfifo "$FIFO"

start()      { QAQH_DATA_DIR="$D/qaqh" "$DAEMON" run </dev/null >>"$D/daemon.out" 2>&1 & echo $!; }
# 必须连 pid 一起核：只判「文件存在」会读到上一轮遗留的 daemon.json。
wait_pid()   { for _ in $(seq 1 40); do grep -q "\"pid\": $1" "$D/qaqh/daemon.json" 2>/dev/null && return 0; sleep 1; done; return 1; }
epoch()      { grep -o '"server_epoch": "[^"]*"' "$D/qaqh/daemon.json" 2>/dev/null | cut -d'"' -f4 | cut -c1-12; }

D1=$(start); wait_pid "$D1" || { echo "daemon#1 未起来"; cat "$D/daemon.out"; exit 1; }
echo "t=0   daemon#1 pid=$D1 epoch=$(epoch)"

# 喂按键：全程持住写端——关掉会让 TUI 的 stdin 立刻 EOF。
( exec 3>"$FIFO"
  sleep "$PROBE_DELAY"; printf '\x12' >&3; echo "t=$PROBE_DELAY  Ctrl+R（daemon 仍 dead，相位应已是 lost）"
  sleep 20;             printf '\x12' >&3; echo "t=$((PROBE_DELAY+20))  Ctrl+R（daemon#2 已起）"
  sleep 40 ) & FEED=$!

QAQH_DATA_DIR="$D/qaqh" timeout 125 script -qec "stty rows 40 cols 130; timeout 120 $TUI" /dev/null \
  <"$FIFO" > "$D/tui.raw" 2>&1 &
TPID=$!
sleep 8;  echo "t=8   杀 daemon#1"; kill -9 "$D1" 2>/dev/null; rm -f "$D/qaqh/daemon.json"
sleep 70; echo "t=78  起 daemon#2"; D2=$(start); wait_pid "$D2" && echo "t=78  daemon#2 pid=$D2 epoch=$(epoch)"

wait $TPID 2>/dev/null; kill "$D2" 2>/dev/null; wait $FEED 2>/dev/null

python3 - "$D/tui.raw" <<'PY'
import re, sys, pathlib
s = re.sub(r"\x1b\[[0-9;?]*[a-zA-Z]", "", pathlib.Path(sys.argv[1]).read_text(errors="replace")).replace("\r", "")
seen = []
for m in re.finditer(r"(● ready|◌ connecting|✗ lost)(\s*[0-9a-f]{6,12})?", s):
    tok = (m.group(1) + (m.group(2) or "")).strip()
    if not seen or seen[-1] != tok:
        seen.append(tok)
print("相位：", " → ".join(seen))
print("重连文案：")
for m in re.finditer(r"[^\n]{0,30}(重连|重建连接)[^\n]{0,40}", s):
    print("  |", m.group(0).strip()[:100])
PY
echo
echo "期望：相位出现两次 ● ready 且 **epoch 不同**（= 换了 daemon 后自愈）；"
echo "      重连文案里有「正在重连」与「重连失败：…」（= Ctrl+R 的动作路径真的跑了）。"
