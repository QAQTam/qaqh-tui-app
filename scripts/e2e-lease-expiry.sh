#!/bin/bash
# 真机端到端：租约过期自愈（台账 U-06 / issue #22 · W-04）。
#
# 为什么需要它：U-06（「会话活着时让租约失效，期望自动重新 open，不出现永久
# `✗ lost`」）挂了很久 —— 单测覆盖不到，因为它要一个**能把租约 TTL 压短的
# daemon**，而连接生命周期在 `qaqh-client` 里、TUI 侧只消费结果。
# 本脚本用 daemon 自带的测试钩子 `QAQH_TEST_LEASE_TTL_MS`
# （`qaqh-runtime/src/ringing/lease_store.rs` + `qaqh-daemon/.../axum_impl/mod.rs`）
# 把 TTL 压到 3s，小于客户端 5s 的续租节拍（`renew_interval_ms` 固定 10s，
# 客户端按 `max(1000, interval/2)` 续租）—— 于是**每次续租都必然失败**，
# 客户端必须在连续 2 次失败后重新 open 换租约。这正是 U-06 要验的路径。
#
# 判据（两条都要成立，缺一即红）：
#   ① **确实发生了重新 open**：经 TCP 计数代理观测
#      `POST /ringing/v1/clients/open` 次数 ≥ 2（首次 open + 至少一次重新协商）。
#      没有这条，脚本会在「TTL 覆盖没生效」时**空过** —— 那正是最该排除的情况。
#   ② **TUI 侧自愈**：相位最终停在 `● ready`，且全程不出现 `✗ lost`。
#
# 用法：scripts/e2e-lease-expiry.sh
# 前置：qaqh-tui 与 qaqh-daemon 均已构建。
#       本脚本**不构建、不改任何生产代码**（后端仓可正处在改造中，
#       只要那个 daemon 二进制存在且能跑）。
# 环境：`DAEMON` / `TUI` / `QAQH_BACKEND_ROOT` 可覆盖；`TTL_MS` / `RUN_SECS` 可调。
#
# 隔离：私有 data root（/tmp 下），不碰你正在用的 daemon 与会话。
# ⚠ Linux 下 data root 的 basename 必须是 `qaqh`（Windows 才是 `.qaqh`）——
#   后端那条 ignored 集成测试在 Linux 上跑不起来正是栽在这里（后端 issue #97）。
#
# 不要用 `pkill -f <模式>` 清理本脚本的进程：脚本自身的命令行就含那些模式。
# 全程按显式 PID 操作（同 e2e-restart.sh 的教训，实测 exit 144）。

set -u

SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
REPO_ROOT=$(cd -- "$SCRIPT_DIR/.." && pwd)
BACKEND_ROOT=${QAQH_BACKEND_ROOT:-$REPO_ROOT/../qaqh-backend}
DAEMON=${DAEMON:-$BACKEND_ROOT/target/debug/qaqh-daemon}
TUI=${TUI:-$REPO_ROOT/target/debug/qaqh-tui}
D=${D:-/tmp/qaqh-e2e-lease}
TTL_MS=${TTL_MS:-3000}
RUN_SECS=${RUN_SECS:-70}

for b in "$DAEMON" "$TUI"; do
  [ -x "$b" ] || { echo "缺少可执行文件：$b"; echo "（本脚本不构建，请先 cargo build）"; exit 1; }
done
command -v python3 >/dev/null 2>&1 || { echo "需要 python3（计数代理 + 输出解析）"; exit 1; }

# ── 隔离的 data root ──────────────────────────────────────────────────────
case "$(uname -s)" in
  MINGW*|MSYS*|CYGWIN*) ROOT_NAME=".qaqh" ;;   # Windows：<home>/.qaqh
  *)                    ROOT_NAME="qaqh"  ;;   # POSIX：父目录任意，basename 必须叫 qaqh
esac
HOME_DIR=$D/home
DATA=$HOME_DIR/$ROOT_NAME

rm -rf "$D"; mkdir -p "$DATA" "$D"

DAEMON_PID=""; PROXY_PID=""; FEED_PID=""
cleanup() {
  [ -n "$DAEMON_PID" ] && kill -9 "$DAEMON_PID" 2>/dev/null
  [ -n "$PROXY_PID" ]  && kill -9 "$PROXY_PID"  2>/dev/null
  [ -n "$FEED_PID" ]   && kill    "$FEED_PID"   2>/dev/null
  return 0
}
trap cleanup EXIT

# ── 起 daemon（唯一非默认处：TTL 压短） ───────────────────────────────────
QAQH_DATA_DIR="$DATA" HOME="$HOME_DIR" USERPROFILE="$HOME_DIR" \
  QAQH_TEST_LEASE_TTL_MS="$TTL_MS" \
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
  sed -n '1,40p' "$D/daemon.out"
  exit 1
fi

read -r EP_HOST EP_PORT < <(python3 - "$DISCOVERY" <<'PY'
import json, sys, urllib.parse
d = json.load(open(sys.argv[1], encoding="utf-8"))
u = urllib.parse.urlparse(d["endpoint"])
print(u.hostname, u.port)
PY
)
echo "daemon pid=$DAEMON_PID  endpoint=$EP_HOST:$EP_PORT  TTL=${TTL_MS}ms"

# ── 计数代理：统计 /clients/open 次数（判据 ① 的硬证据） ─────────────────
cat >"$D/proxy.py" <<'PY'
import socket, sys, threading, pathlib

port_file, target_host, target_port, count_file = sys.argv[1], sys.argv[2], int(sys.argv[3]), sys.argv[4]
NEEDLE = b"POST /ringing/v1/clients/open"
lock = threading.Lock()
count = 0


def bump(n: int) -> None:
    global count
    with lock:
        count += n
        pathlib.Path(count_file).write_text(str(count), encoding="utf-8")


def pump(src, dst, sniff: bool) -> None:
    tail = b""
    try:
        while True:
            data = src.recv(65536)
            if not data:
                break
            if sniff:
                tail = (tail + data)[-8192:]
                n = tail.count(NEEDLE)
                if n:
                    bump(n)
                    tail = b""          # 计数后清尾，避免跨块重复计数
            dst.sendall(data)
    except OSError:
        pass
    finally:
        try:
            dst.shutdown(socket.SHUT_WR)
        except OSError:
            pass


def serve(client) -> None:
    try:
        upstream = socket.create_connection((target_host, target_port))
    except OSError:
        client.close()
        return
    threading.Thread(target=pump, args=(client, upstream, True), daemon=True).start()
    pump(upstream, client, False)
    client.close()
    upstream.close()


srv = socket.socket()
srv.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
srv.bind(("127.0.0.1", int(port_file) if port_file.isdigit() else 0))
pathlib.Path(sys.argv[5]).write_text(str(srv.getsockname()[1]), encoding="utf-8")
pathlib.Path(count_file).write_text("0", encoding="utf-8")
srv.listen(64)
while True:
    conn, _ = srv.accept()
    threading.Thread(target=serve, args=(conn,), daemon=True).start()
PY

python3 "$D/proxy.py" 0 "$EP_HOST" "$EP_PORT" "$D/opens" "$D/proxy.port" >>"$D/proxy.out" 2>&1 &
PROXY_PID=$!
for _ in $(seq 1 50); do [ -s "$D/proxy.port" ] && break; sleep 0.1; done
if [ ! -s "$D/proxy.port" ]; then
  echo "计数代理未起来"; sed -n '1,20p' "$D/proxy.out"; exit 1
fi
PROXY_PORT=$(cat "$D/proxy.port")
echo "计数代理 127.0.0.1:$PROXY_PORT → $EP_HOST:$EP_PORT"

# 把 discovery 的 endpoint 改指代理：TUI 全程经代理访问 daemon。
# （daemon.json 只在启动时写一次、退出时删，不会被 daemon 覆盖。）
python3 - "$DISCOVERY" "$PROXY_PORT" <<'PY'
import json, sys
path, port = sys.argv[1], sys.argv[2]
d = json.load(open(path, encoding="utf-8"))
d["endpoint"] = f"http://127.0.0.1:{port}"
with open(path, "w", encoding="utf-8") as fh:
    json.dump(d, fh, indent=2, ensure_ascii=False)
PY

# ── 跑 TUI（pty；stdin 用 FIFO 持住，否则 EventStream 立刻 EOF） ─────────
FIFO=$D/in
mkfifo "$FIFO"
( exec 3>"$FIFO"; sleep "$RUN_SECS" ) & FEED_PID=$!

QAQH_DATA_DIR="$DATA" HOME="$HOME_DIR" USERPROFILE="$HOME_DIR" \
  timeout $((RUN_SECS + 15)) script -qec "stty rows 40 cols 130; timeout $RUN_SECS $TUI" /dev/null \
  <"$FIFO" >"$D/tui.raw" 2>&1 &
TUI_PID=$!
echo "TUI 跑 ${RUN_SECS}s（期间应发生多次租约重新协商）…"
wait "$TUI_PID" 2>/dev/null
wait "$FEED_PID" 2>/dev/null
FEED_PID=""

OPENS=$(cat "$D/opens" 2>/dev/null || echo 0)

# ── 判据 ─────────────────────────────────────────────────────────────────
# 不用 eval：相位 token 里含 ● / → 等字符，eval 会把值当命令解析（踩过）。
python3 - "$D/tui.raw" "$D/phases" "$D/verdict" <<'PY'
import re, sys, pathlib

raw = pathlib.Path(sys.argv[1]).read_text(errors="replace")
s = re.sub(r"\x1b\[[0-9;?]*[a-zA-Z]", "", raw).replace("\r", "")

phases = []
for m in re.finditer(r"(● ready|◌ connecting|✗ lost)(\s*[0-9a-f]{6,12})?", s):
    tok = (m.group(1) + (m.group(2) or "")).strip()
    if not phases or phases[-1] != tok:
        phases.append(tok)

last = phases[-1] if phases else ""
pathlib.Path(sys.argv[2]).write_text("\n".join(phases), encoding="utf-8")
pathlib.Path(sys.argv[3]).write_text(
    f"{len(phases)} "
    f"{'1' if last.startswith('● ready') else '0'} "
    f"{'1' if any(p.startswith('✗ lost') for p in phases) else '0'}\n",
    encoding="utf-8",
)
PY

read -r PHASE_COUNT LAST_READY SAW_LOST <"$D/verdict"
PHASES_DISPLAY=$(awk 'NR>1{printf " → "} {printf "%s", $0} END{print ""}' "$D/phases" 2>/dev/null)
LAST_PHASE=$(tail -n 1 "$D/phases" 2>/dev/null)

echo
echo "相位序列（去重后 $PHASE_COUNT 段）：${PHASES_DISPLAY:-（未捕获到相位）}"
echo "open 次数：$OPENS（首次 open = 1，其余为租约重新协商）"

FAIL=0
if [ "${OPENS:-0}" -lt 2 ]; then
  echo "✗ 判据① 未满足：只观测到 ${OPENS} 次 /clients/open —— 租约从未重新协商。"
  echo "  这通常意味着 TTL 覆盖没生效（测试会空过），请先排查 daemon 是否认 QAQH_TEST_LEASE_TTL_MS。"
  FAIL=1
else
  echo "✓ 判据① 重新 open 发生（$OPENS 次 /clients/open）"
fi

if [ "$LAST_READY" = 1 ]; then
  echo "✓ 判据② TUI 最终停在 ● ready"
else
  echo "✗ 判据② TUI 最终相位不是 ● ready（末相位：${LAST_PHASE:-（未捕获到相位）}）"
  FAIL=1
fi

if [ "$SAW_LOST" = 1 ]; then
  echo "✗ 判据② 全程出现过 ✗ lost（U-06 要排除的正是它）"
  FAIL=1
else
  echo "✓ 判据② 全程未出现 ✗ lost"
fi

echo
if [ "$FAIL" = 0 ]; then
  echo "全部通过：租约过期后客户端自动重新 open，TUI 自愈到 ready。"
else
  echo "未通过。原始材料：$D/tui.raw（TUI）、$D/daemon.out（daemon）"
fi
exit "$FAIL"
