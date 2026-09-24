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
SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
REPO_ROOT=$(cd -- "$SCRIPT_DIR/.." && pwd)
# 默认吃**锚点 worktree**（TUI 钉的 rev，见 scripts/ci-linux.sh 的 QAQH_BACKEND_REV），
# 而不是开发者正在用的 ../qaqh-backend 工作树——否则 e2e 会跑到别人分支构建的
# daemon 上。原先这里硬编码 $HOME/Projects/...，在非该布局的机器上直接找不到。
BACKEND_ROOT=${QAQH_BACKEND_ROOT:-$REPO_ROOT/../qaqh-backend-anchor}
DAEMON=${DAEMON:-$BACKEND_ROOT/target/debug/qaqh-daemon}
TUI=${TUI:-$REPO_ROOT/target/debug/qaqh-tui}
D=/tmp/qaqh-e2e-restart
PROBE_DELAY=${PROBE_DELAY:-30}   # 杀 daemon 后等多久按 Ctrl+R（须 > STALL_AFTER=20s）
# ⚠ 本 harness 断言的是**状态栏相位面**（`● ready` → `✗ lost` → 新 epoch 的
# `● ready`）与重连文案，那是 v1 的呈现：v2 Agent View 只在有活动会话时才画
# `status_line`，空 data root 下拿不到相位 token。故默认走 `--v1` 回退路径；
# 连接/重连行为本身与 UI 无关。调用方可用 `TUI_ARGS=…` 覆盖。
TUI_ARGS=${TUI_ARGS:---v1}
export TUI_ARGS

for b in "$DAEMON" "$TUI"; do
  [ -x "$b" ] || { echo "缺少可执行文件：$b（先 cargo build）"; exit 1; }
done

rm -rf "$D"; mkdir -p "$D/qaqh"

start()      { QAQH_DATA_DIR="$D/qaqh" "$DAEMON" run </dev/null >>"$D/daemon.out" 2>&1 & echo $!; }
# 必须连 pid 一起核：只判「文件存在」会读到上一轮遗留的 daemon.json。
wait_pid()   { for _ in $(seq 1 40); do grep -q "\"pid\": $1" "$D/qaqh/daemon.json" 2>/dev/null && return 0; sleep 1; done; return 1; }
epoch()      { grep -o '"server_epoch": "[^"]*"' "$D/qaqh/daemon.json" 2>/dev/null | cut -d'"' -f4 | cut -c1-12; }

D1=$(start); wait_pid "$D1" || { echo "daemon#1 未起来"; cat "$D/daemon.out"; exit 1; }
echo "t=0   daemon#1 pid=$D1 epoch=$(epoch)"

# 按键由驱动按时间表注入（两次 Ctrl+R），不再用 FIFO：哑驱动收不到 `ESC[6n`
# 应答，默认 V2 Agent View 会初始化即 panic（假红）。见 `scripts/lib/pty-driver.py`。
echo "t=$PROBE_DELAY  Ctrl+R（daemon 仍 dead，相位应已是 lost）"
echo "t=$((PROBE_DELAY+13))  Ctrl+R（daemon#2 已起）"

QAQH_DATA_DIR="$D/qaqh" python3 "$REPO_ROOT/scripts/lib/pty-driver.py" \
  --tui "$TUI" --raw "$D/tui.raw" --seconds 70 \
  --key "$PROBE_DELAY:12" --key "$((PROBE_DELAY+13)):12" &
TPID=$!
sleep 8;  echo "t=8   杀 daemon#1"; kill -9 "$D1" 2>/dev/null; rm -f "$D/qaqh/daemon.json"
sleep 29; echo "t=37  起 daemon#2"; D2=$(start); wait_pid "$D2" && echo "t=37  daemon#2 pid=$D2 epoch=$(epoch)"

wait $TPID 2>/dev/null; kill "$D2" 2>/dev/null

python3 - "$D/tui.raw" <<'PY'
import re, sys, pathlib
s = re.sub(r"\x1b\[[0-9;?]*[a-zA-Z]", "", pathlib.Path(sys.argv[1]).read_text(errors="replace")).replace("\r", "")
seen = []
# 相位 token 两个 UI 都要认：v1 状态栏是 `◌ connecting`，v2 Agent View 的
# `status_line` 是 `○ opening` / `● ready` / `● degraded` / `✗ lost`。
for m in re.finditer(
    r"(● ready|● degraded|○ opening|◌ connecting|✗ lost)(\s*[0-9a-f]{6,12})?", s
):
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
