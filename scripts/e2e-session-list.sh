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
BACKEND_ROOT=${QAQH_BACKEND_ROOT:-$REPO_ROOT/../qaqh-backend-anchor}
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

FIFO=$D/in; mkfifo "$FIFO"
( exec 3>"$FIFO"; sleep 40 ) & FEED=$!

PID=$(start); wait_pid "$PID" || { echo "daemon 未起来"; cat "$D/daemon.out"; exit 1; }
echo "daemon pid=$PID"

QAQH_DATA_DIR="$D/qaqh" timeout 55 script -qec "stty rows 40 cols 130; timeout 50 $TUI" /dev/null \
  <"$FIFO" > "$D/tui.raw" 2>&1 &
TPID=$!
wait $TPID 2>/dev/null
kill "$PID" 2>/dev/null; wait "$FEED" 2>/dev/null

python3 - "$D/tui.raw" <<'PY'
import re, sys, pathlib
# 去 ANSI 后**整块**搜：TUI 用光标定位重绘，落盘文本不是按行分帧的，
# 按行切会漏（实测：列表内容与更早的「加载中…」帧粘在同一「行」里）。
s = re.sub(r"\x1b\[[0-9;?]*[a-zA-Z]", "", pathlib.Path(sys.argv[1]).read_text(errors="replace")).replace("\r", "")

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
