#!/bin/bash
# 真机端到端：`session.list` 的**类型化条目**在首页真的渲染得出来（G2）。
#
# 为什么需要它：G2 把 TUI 的 128 行手解换成权威类型 `SessionListEntry`。
# 单测能锁住「wire 键集合」与「字段可达」（qaqh-types），编译期能锁住
# 「消费侧字段名对得上」——但**没有一条能证明首页真的把这几个字段显示出来**。
# 本脚本补这一段：往隔离 data root 里放几个手工 meta.json，跑真 daemon + 真 TUI，
# 断言首页渲染出的文本。
#
# 三个会话刻意覆盖 `display_title()` 的三级回退：
#   A 有 title        → 显示 title（且**不得**显示 last_summary）
#   B 无 title 有 cwd → 显示 cwd 尾段
#   C 都没有          → 显示 seed
#
# 用法：scripts/e2e-session-list.sh
# 前置：两仓均已 `cargo build`。全程用隔离 `QAQH_DATA_DIR`，不碰在用的 daemon 与会话。
# 注意：不要 `pkill -f <模式>` 清理（本脚本命令行自身含那些模式，会连自己一起杀）。

set -u
SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
REPO_ROOT=$(cd -- "$SCRIPT_DIR/.." && pwd)
# 默认吃**锚点 worktree**（TUI 钉的 rev，见 scripts/ci-linux.sh 的 QAQH_BACKEND_REV），
# 而不是开发者正在用的 ../qaqh-backend 工作树——否则 e2e 会跑到别人分支构建的
# daemon 上。原先这里硬编码 $HOME/Projects/...，在非该布局的机器上直接找不到。
BACKEND_ROOT=${QAQH_BACKEND_ROOT:-$REPO_ROOT/../qaqh-backend}
DAEMON=${DAEMON:-$BACKEND_ROOT/target/debug/qaqh-daemon}
TUI=${TUI:-$REPO_ROOT/target/debug/qaqh-tui}
D=/tmp/qaqh-e2e-session-list
SESS=$D/qaqh/sessions

for b in "$DAEMON" "$TUI"; do
  [ -x "$b" ] || { echo "缺少可执行文件：$b（先 cargo build）"; exit 1; }
done

rm -rf "$D"; mkdir -p "$SESS"

# meta.json 直接落盘：SessionManager::list() 在索引为空时会回退到扫描会话目录，
# 故无需伪造索引（伪造反而会掩盖真实读取路径）。
put() { # put <seed> <extra-json>
  mkdir -p "$SESS/$1"
  cat > "$SESS/$1/meta.json" <<JSON
{"seed":"$1","created_at":1757900000000,"updated_at":1757900000000,"model":"m1",
 "message_count":3,"turn_count":2,"mode":0,"archived":false,"ephemeral":false$2}
JSON
}
# A：title 优先，last_summary 是干扰项（它**不得**出现在标题里）
put aaaa1111 ',"title":"Bun 引导 daemon","last_summary":"最后一条回复首行","cwd":"/home/x/Projects/qaqh-backend"'
# B：无 title → cwd 尾段
put bbbb2222 ',"cwd":"/home/x/work/demo-proj"'
# C：都没有 → seed
put cccc3333 ''

start() { QAQH_DATA_DIR="$D/qaqh" "$DAEMON" run </dev/null >>"$D/daemon.out" 2>&1 & echo $!; }
wait_pid() { for _ in $(seq 1 40); do grep -q "\"pid\": $1" "$D/qaqh/daemon.json" 2>/dev/null && return 0; sleep 1; done; return 1; }

PID=$(start); wait_pid "$PID" || { echo "daemon 未起来"; cat "$D/daemon.out"; exit 1; }
echo "daemon pid=$PID"

# 真 PTY 驱动：默认 V2 Agent View 初始化时会发 `ESC[6n` 光标查询，哑驱动
# （`script(1)`）无人应答 → TUI 初始化即 panic，本脚本会假红。见
# `scripts/lib/pty-driver.py` 顶部说明。
#
# ⚠ 会话列表在 v2 里**不再挂在首屏**：默认 Agent View 首屏是空会话工作区，
# 列表走 `Ctrl+L`（首屏页脚有该提示）。旧版本脚本假设首屏即列表，默认切到
# Agent View 后会假红，故这里显式按一次 Ctrl+L（0x0c）。
QAQH_DATA_DIR="$D/qaqh" python3 "$REPO_ROOT/scripts/lib/pty-driver.py" \
  --tui "$TUI" --raw "$D/tui.raw" --seconds 20 --key 3:0c --quit
kill "$PID" 2>/dev/null

python3 - "$D/tui.raw" "$REPO_ROOT" <<'PY'
import sys, pathlib

# ⚠ 必须**重建屏幕**再断言，不能"去 ANSI 后 grep"：TUI 是按光标定位只写变化
# 单元格的差分流，没变的空格根本不在字节流里，于是 `Bun 引导 daemon` 会被拼成
# `Bun引导daemon` 而假红（实测过）。详见 scripts/lib/vt_screen.py。
sys.path.insert(0, str(pathlib.Path(sys.argv[2]) / "scripts" / "lib"))
from vt_screen import render_text

s = render_text(pathlib.Path(sys.argv[1]).read_text(errors="replace"))

checks = [
    ("A 标题优先（title）",      "Bun 引导 daemon"),
    ("B 回退 cwd 尾段",          "demo-proj"),
    ("C 回退 seed",              "cccc3333"),
]
ok = True
for name, needle in checks:
    hit = needle in s
    ok &= hit
    print(f"  [{'✓' if hit else '✗'}] {name}: 期望文本 {needle!r}")
# 反例：last_summary 不得当标题（它是「最后一条回复首行」的预览）
bad = "最后一条回复首行" in s
ok &= not bad
print(f"  [{'✓' if not bad else '✗'}] last_summary 未参与标题")
print("RESULT:", "PASS" if ok else "FAIL")
sys.exit(0 if ok else 1)
PY
