#!/bin/bash
# 真机端到端：默认 Agent View 下 `Ctrl+N` 必须**真的把新会话开出来**。
#
# 为什么需要它：这是默认 UI 的**第一个动作**，却曾经整条失效——
# 实测（真 daemon，锚点 b77c251）按 Ctrl+N 后：
#
#   - daemon 侧完全正常：命令收据 `state=succeeded`，journal 里
#     `session_state_changed/created` 带**正确**的 `causation_id == command_id`；
#   - TUI 侧什么都没发生：不开 tab、不 toast（首屏会话列表 3s
#     自动刷新把「最近会话 — 1 个」显示出来，遮住了故障）；
#   - 默认 Agent View 首屏**没有列表面**，于是表现为「按了没反应」——
#     再按一次就**又建一个**（实测连按两次 = 磁盘上 2 个会话，界面全程不变）。
#
# 根因是「新会话靠 `SessionStateEvent::Created` 那条可靠事件开 tab」这条路径在
# 实践里不成立，而它是唯一的开 tab 路径。修法是列表兜底：有在途 create 时，
# 把列表里 `created_at` 最新的、本地还没 tab 的 seed 开出来（见
# `src/app/mod.rs` 的 `ActionResult::SessionList` 分支）。
#
# 判据（缺一即红）：
#   ① 屏幕上出现 composer（`❯`）与状态行（`● ready`）= 会话真的打开了；
#   ② 只建了 **1** 个会话 = 没有「按了没反应 → 反复按 → 建一堆」；
#   ③ 出现「新会话已创建」文案。
#
# 用法：scripts/e2e-new-session.sh
# 前置：两仓均已 `cargo build`。全程用隔离 data root，不碰在用的 daemon 与会话。
# 注意：不要 `pkill -f <模式>` 清理（本脚本命令行自身含那些模式，会连自己一起杀）。

set -u
SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
REPO_ROOT=$(cd -- "$SCRIPT_DIR/.." && pwd)
# 默认吃**锚点 worktree**（TUI 钉的 rev，见 scripts/ci-linux.sh 的 QAQH_BACKEND_REV），
# 而不是开发者正在用的 ../qaqh-backend 工作树——否则 e2e 会跑到别人分支构建的
# daemon 上，与本仓门禁的锚点不是同一个东西。
BACKEND_ROOT=${QAQH_BACKEND_ROOT:-$REPO_ROOT/../qaqh-backend}
DAEMON=${DAEMON:-$BACKEND_ROOT/target/debug/qaqh-daemon}
TUI=${TUI:-$REPO_ROOT/target/debug/qaqh-tui}
D=${D:-/tmp/qaqh-e2e-new-session}
RUN_SECS=${RUN_SECS:-18}
# 第几秒按 Ctrl+N（0x0e）。留够首帧与订阅建立的时间。
NEW_AT=${NEW_AT:-4}

# ⚠ 守卫：`D` 可被环境覆盖，而下面要 `rm -rf "$D"`（同 e2e-lease-expiry.sh 的教训）。
case "$D" in
  /tmp/*|/var/tmp/*) ;;
  *) echo "拒绝：D 必须落在 /tmp 或 /var/tmp 下（当前：$D）——本脚本会对它 rm -rf" >&2; exit 1 ;;
esac

for b in "$DAEMON" "$TUI"; do
  [ -x "$b" ] || { echo "缺少可执行文件：$b"; echo "（本脚本不构建，请先 cargo build）"; exit 1; }
done
command -v python3 >/dev/null 2>&1 || { echo "需要 python3（PTY 驱动 + 屏幕重建）"; exit 1; }

rm -rf "$D"; mkdir -p "$D/qaqh"
DATA=$D/qaqh

DAEMON_PID=""
cleanup() {
  [ -n "$DAEMON_PID" ] && kill -9 "$DAEMON_PID" 2>/dev/null
  return 0
}
trap cleanup EXIT

QAQH_DATA_DIR="$DATA" "$DAEMON" run </dev/null >>"$D/daemon.out" 2>&1 &
DAEMON_PID=$!
ok=0
for _ in $(seq 1 40); do
  # 必须连 pid 一起核：只判「文件存在」会读到上一轮遗留的 daemon.json。
  if grep -q "\"pid\": $DAEMON_PID" "$DATA/daemon.json" 2>/dev/null; then ok=1; break; fi
  sleep 1
done
if [ "$ok" != 1 ]; then
  echo "daemon 未在 40s 内写出 discovery（pid=$DAEMON_PID）"
  sed -n '1,30p' "$D/daemon.out"
  exit 1
fi
echo "daemon pid=$DAEMON_PID 就绪；跑默认 Agent View ${RUN_SECS}s，第 ${NEW_AT}s 按 Ctrl+N…"

# 真 PTY 驱动（必须答 `ESC[6n`，见 scripts/lib/pty-driver.py 顶部说明）。
QAQH_DATA_DIR="$DATA" python3 "$REPO_ROOT/scripts/lib/pty-driver.py" \
  --tui "$TUI" --raw "$D/tui.raw" --seconds "$RUN_SECS" \
  --key "$NEW_AT:0e" --quit

SESSIONS=$(find "$DATA/sessions" -mindepth 1 -maxdepth 1 -type d 2>/dev/null | wc -l)

python3 - "$D/tui.raw" "$REPO_ROOT" "$SESSIONS" <<'PY'
import sys, pathlib

# ⚠ 必须**重建屏幕**再断言，不能"去 ANSI 后 grep"：TUI 是按光标定位只写变化
# 单元格的差分流，没变的空格根本不在字节流里。详见 scripts/lib/vt_screen.py。
sys.path.insert(0, str(pathlib.Path(sys.argv[2]) / "scripts" / "lib"))
from vt_screen import render_text

raw = pathlib.Path(sys.argv[1]).read_text(errors="replace")
screen = render_text(raw)
sessions = int(sys.argv[3])

checks = [
    ("① composer 出现（会话已打开）", "❯" in screen),
    ("① 状态行出现（● ready）", "● ready" in screen),
    ("② 只建了 1 个会话", sessions == 1),
    ("③ 出现「新会话已创建」文案", "新会话已创建" in raw),
]
ok = True
for name, hit in checks:
    ok &= hit
    print(f"  [{'✓' if hit else '✗'}] {name}" + (f"（实测 {sessions} 个）" if "只建了" in name else ""))
if not ok:
    print("--- 重建后的屏幕 ---")
    print(screen[:1200])
print("RESULT:", "PASS" if ok else "FAIL")
sys.exit(0 if ok else 1)
PY
