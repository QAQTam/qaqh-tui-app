#!/bin/bash
# 真机端到端：V2 Agent View 在 **tmux**（真实 multiplexer）里的链路
# （M6.3 / 兼容矩阵 §3 的 tmux 行）。
#
# 与另两个脚本的分工：
#   `e2e-v2-terminal-matrix.sh`  自开 PTY + 解析字节流 —— 环境能力组合；
#   `e2e-v2-real-terminal.sh`   真终端模拟器（kitty）—— 模拟器解出来的屏幕；
#   `e2e-v2-tmux.sh`（本脚本）  真 multiplexer —— **scrollback 语义**。
#
# 为什么 tmux 值得单独测：TUI 的 scrollback 在 tmux 里由 tmux 自己持有
# （`history-limit` + pane history），而不是终端模拟器。`capture-pane -S -`
# 能直接读回「可见区 + 历史」，是这条语义最权威的观测面。历史上
# `ClearType::Purge`、alternate-screen 进出、嵌套时的清理行为都在这层出过问题。
#
# 判据全部来自 tmux 自己的 pane 回读，不看 TUI 自述。
#
# 用法：scripts/e2e-v2-tmux.sh
# 前置：qaqh-daemon 与 qaqh-tui 已构建；`tmux` 在 PATH 上。
# 缺 tmux 时**显式 SKIP**（exit 0），不假装通过。
# 隔离：私有 tmux server（`-L` 独立 socket）+ 私有 QAQH_DATA_DIR，不碰用户的 tmux。

set -u

SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
REPO_ROOT=$(cd -- "$SCRIPT_DIR/.." && pwd)
BACKEND_ROOT=${QAQH_BACKEND_ROOT:-$REPO_ROOT/../qaqh-backend}
DAEMON=${DAEMON:-$BACKEND_ROOT/target/debug/qaqh-daemon}
TUI=${TUI:-$REPO_ROOT/target/debug/qaqh-tui}
D=${D:-/tmp/qaqh-e2e-v2-tmux}
# 独立 socket：绝不碰用户正在用的 tmux server。
TMUX_SOCK=${TMUX_SOCK:-qaqh-e2e-$$}
TMUX_SESSION=qaqh-e2e

for binary in "$DAEMON" "$TUI"; do
    [ -x "$binary" ] || {
        echo "缺少可执行文件：$binary（先 cargo build）" >&2
        exit 1
    }
done

if ! command -v tmux >/dev/null 2>&1; then
    echo "SKIP: 未安装 tmux —— tmux 行需要真实 multiplexer"
    exit 0
fi

case "$D" in
    /tmp/*) ;;
    *)
        echo "D 必须位于 /tmp 下：$D" >&2
        exit 1
        ;;
esac

python3 - "$D" <<'PY'
import pathlib
import shutil
import sys

d = pathlib.Path(sys.argv[1])
if d.exists():
    shutil.rmtree(d)
(d / "qaqh").mkdir(parents=True)
PY

# ── 隔离 daemon ────────────────────────────────────────────────────────
setsid env QAQH_DATA_DIR="$D/qaqh" "$DAEMON" run </dev/null >>"$D/daemon.out" 2>&1 &
DAEMON_PID=$!
for _ in $(seq 1 40); do
    if grep -q "\"pid\": $DAEMON_PID" "$D/qaqh/daemon.json" 2>/dev/null; then
        break
    fi
    sleep 1
done
if ! grep -q "\"pid\": $DAEMON_PID" "$D/qaqh/daemon.json" 2>/dev/null; then
    echo "daemon 未起来；日志：" >&2
    cat "$D/daemon.out" >&2
    exit 1
fi
echo "daemon pid=$DAEMON_PID data=$D"

cleanup() {
    tmux -L "$TMUX_SOCK" kill-server >/dev/null 2>&1 || true
    # kill-server 偶发会留下死 socket 文件，顺手清掉（只删本脚本的独立 socket，
    # 绝不碰用户自己的 `default`）。
    python3 - "$TMUX_SOCK" <<'PYEOF' 2>/dev/null || true
import os
import pathlib
import sys

pathlib.Path(f"/tmp/tmux-{os.getuid()}/{sys.argv[1]}").unlink(missing_ok=True)
PYEOF
    kill "$DAEMON_PID" 2>/dev/null || true
    wait "$DAEMON_PID" 2>/dev/null || true
}
trap cleanup EXIT

D="$D" TUI="$TUI" TMUX_SOCK="$TMUX_SOCK" TMUX_SESSION="$TMUX_SESSION" python3 - <<'PY'
import json
import os
import pathlib
import subprocess
import time
import uuid
from urllib.request import Request, urlopen

D = pathlib.Path(os.environ["D"])
DATA = D / "qaqh"
SOCK = os.environ["TMUX_SOCK"]
SESSION = os.environ["TMUX_SESSION"]
TUI = os.environ["TUI"]
DISCOVERY = DATA / "daemon.json"

def tmux(*args):
    return subprocess.run(
        ("tmux", "-L", SOCK, *args), capture_output=True, text=True, timeout=30
    )

def pane_text(history=False, ansi=False):
    args = ["capture-pane", "-p", "-t", SESSION]
    if history:
        args.append("-S")  # 从历史起点开始 = 可见区 + scrollback
        args.append("-")
    if ansi:
        args.append("-e")
    return tmux(*args).stdout

def pane_geometry():
    out = tmux(
        "display", "-p", "-t", SESSION, "#{pane_width}x#{pane_height}"
    ).stdout.strip()
    return out

def post_json(url, payload, headers=None):
    body = json.dumps(payload).encode()
    request = Request(url, data=body, method="POST")
    request.add_header("Content-Type", "application/json")
    for key, value in (headers or {}).items():
        request.add_header(key, value)
    with urlopen(request, timeout=10) as response:
        return json.loads(response.read())

(DATA / "config.toml").write_text(
    'provider_id = "openai"\nactive_profile = "default"\npermission_level = 2\n\n'
    '[profiles.default]\nmodel = "fake-model"\nmax_tokens = 4096\neffort = "low"\n'
    'context_limit = 100000\nbase_url = "http://127.0.0.1:1/v1"\nendpoint = "openai"\n'
)

discovery = json.loads(DISCOVERY.read_text())
endpoint = discovery["endpoint"].rstrip("/")
token = discovery["token"]
client_instance_id = str(uuid.uuid4())
opened = post_json(
    f"{endpoint}/ringing/v1/clients/open",
    {"schema": "qaqh.Ringing", "version": 1, "client_instance_id": client_instance_id},
    {"Authorization": f"Bearer {token}"},
)
client_session_id = opened["client_session_id"]
post_json(
    f"{endpoint}/ringing/v1/commands/control",
    {
        "schema": "qaqh.Ringing",
        "version": 1,
        "channel": "control",
        "command_id": str(uuid.uuid4()),
        "client_instance_id": client_instance_id,
        "client_session_id": client_session_id,
        "command": {
            "channel": "control",
            "type": "session_create",
            "close_current": False,
            "cwd": None,
            "tool_mode": None,
            "custom_tools": [],
        },
    },
    {
        "Authorization": f"Bearer {token}",
        "X-QAQH-Client-Session-Id": client_session_id,
    },
)
seed = None
deadline = time.monotonic() + 20
while time.monotonic() < deadline and not seed:
    for meta in (DATA / "sessions").glob("*/meta.json"):
        try:
            seed = json.loads(meta.read_text()).get("seed")
        except (OSError, json.JSONDecodeError):
            continue
        if seed:
            break
    time.sleep(0.2)
if not seed:
    raise RuntimeError("session_create 没产出会话")
print(f"created seed={seed}")

# ── 在 tmux pane 里跑 TUI ──────────────────────────────────────────────
# 横幅**先打**再灌 40 行 filler：横幅必然落进 tmux 的 history，scrollback
# 断言才有意义（放最后只会留在可见区，测的是屏幕不是历史）。
# `unset NO_COLOR` 同 kitty 脚本：ambient NO_COLOR=1 会让整屏 Color::Reset。
BANNER = f"TMUX-BANNER-{uuid.uuid4().hex[:8]}"
wrapper = D / "run-in-tmux.sh"
wrapper.write_text(
    "#!/bin/bash\n"
    "unset NO_COLOR\n"
    "export COLORTERM=truecolor\n"
    f'export QAQH_DATA_DIR="{DATA}"\n'
    f'echo "{BANNER}"\n'
    'for i in $(seq 1 40); do echo "filler-$i"; done\n'
    f'"{TUI}" --no-spawn\n'
    'echo "TUI_EXIT=$?"\n'
    "sleep 120\n"
)
wrapper.chmod(0o755)

# `-f /dev/null` 忽略用户配置；`-x/-y` 固定初始尺寸。
created = tmux(
    "-f",
    "/dev/null",
    "new-session",
    "-d",
    "-x",
    "120",
    "-y",
    "32",
    "-s",
    SESSION,
    str(wrapper),
)
if created.returncode != 0:
    print("SKIP: tmux 起不来")
    print(created.stderr.strip()[:400])
    raise SystemExit(0)

checks = []
def check(name, passed, detail=""):
    checks.append((name, bool(passed), detail))
    return passed

def wait_for(predicate, timeout=25.0, interval=0.4):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if predicate():
            return True
        time.sleep(interval)
    return False

# 0) scrollback：横幅必须能从 `-S -`（历史）读回，且**不在可见区**里。
#    这就是 tmux 这层最该验证的语义。
def banner_in_history():
    return BANNER in pane_text(history=True)

if not wait_for(banner_in_history):
    check("scrollback：横幅进入 tmux history", False, f"banner={BANNER}")
else:
    in_history = BANNER in pane_text(history=True)
    visible = BANNER in pane_text(history=False)
    check(
        "scrollback：横幅已滚出可见区但仍在 history 里",
        in_history and not visible,
        f"history={in_history} visible={visible}",
    )
    (D / "pane-history.txt").write_text(pane_text(history=True))

# 1) 启动渲染：Ctrl+L 打开会话列表 → Enter 打开会话 → 应有 composer / 状态行
tmux("send-keys", "-t", SESSION, "C-l")
time.sleep(2.0)
tmux("send-keys", "-t", SESSION, "Enter")
wait_for(lambda: "❯" in pane_text())
time.sleep(1.0)
text = pane_text()
check("启动渲染：composer 提示符可见", "❯" in text, repr(text[-200:]))
check("启动渲染：无 panic", "panicked at" not in text, "")
check("启动渲染：tmux pane 尺寸符合预期", pane_geometry() == "120x32", pane_geometry())

# 2) truecolor：tmux 透传的 24-bit SGR。
#    tmux 会用 T.416 冒号形式（`38:2:`），和 kitty 一样——两种都认。
ansi = pane_text(ansi=True)
(D / "pane.ansi.txt").write_text(ansi)
check(
    "truecolor：tmux 回读含 24-bit SGR",
    any(marker in ansi for marker in ("38;2;", "48;2;", "38:2:", "48:2:")),
    "（读回里没有任何 24-bit SGR 说明 tmux 侧没透传真彩色）",
)

# 2.5) 鼠标/复制：tmux 自己就知道应用有没有申请鼠标追踪
#      （`#{mouse_any_flag}` 在应用发出 `?1000h/?1002h/?1003h/…` 后变 1）。
#      这比扫原始字节流更准——是 multiplexer 侧的权威判定，不是字符串匹配。
#      开了就吃掉终端原生选择/复制，与 README 的承诺冲突。
mouse_flag = tmux("display", "-p", "-t", SESSION, "#{mouse_any_flag}").stdout.strip()
check(
    "鼠标/复制：TUI 未向 tmux 申请鼠标追踪",
    mouse_flag == "0",
    f"mouse_any_flag={mouse_flag}",
)

# 3) resize：真改 pane 尺寸（tmux 会发 SIGWINCH 给 pane 里的 TUI）。
before = pane_geometry()
resized = tmux("resize-window", "-t", SESSION, "-x", "96", "-y", "28")
time.sleep(2.5)
after = pane_geometry()
check(
    "resize：pane 尺寸已变（真实 SIGWINCH 路径）",
    before != after and after == "96x28",
    f"{before} -> {after} rc={resized.returncode}",
)
text_after = pane_text()
check("resize：重排后 UI 仍在（composer 可见）", "❯" in text_after, "")
check("resize：重排后无 panic", "panicked at" not in text_after, "")

# 4) 退出：Ctrl+Q → 退出码回显 0。
tmux("send-keys", "-t", SESSION, "C-q")
exit_text = ""
deadline = time.monotonic() + 20
while time.monotonic() < deadline:
    exit_text = pane_text(history=True)
    if "TUI_EXIT=" in exit_text:
        break
    time.sleep(0.4)
check("退出：Ctrl+Q 后回显 TUI_EXIT=0", "TUI_EXIT=0" in exit_text, repr(exit_text[-160:]))

ok = True
for name, passed, detail in checks:
    ok &= passed
    suffix = f"  {detail}" if (detail and not passed) else ""
    print(f"  [{'✓' if passed else '✗'}] {name}{suffix}")
print("RESULT:", "PASS" if ok else "FAIL")
raise SystemExit(0 if ok else 1)
PY
