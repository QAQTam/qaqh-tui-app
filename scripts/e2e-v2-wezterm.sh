#!/bin/bash
# 真机端到端：V2 Agent View 在 **WezTerm**（真实 GPU 加速终端模拟器）里的链路
# （M6.3 / 兼容矩阵 §3 的 WezTerm 行）。
#
# 与 kitty 脚本同构：判据取自模拟器的屏幕回读（`wezterm cli get-text`），
# 不看 TUI 自述。
#
# ⚠️ 两个 WezTerm 特有的坑（都实测过）：
#
# 1. **必须直连 GUI 自己的 socket**。`wezterm cli --class X ...` 在本版本里
#    并不会去找 GUI 实例，而是连默认路径 `/run/user/$UID/wezterm/sock`；
#    那儿没有 server 时它会**自作主张 spawn 一个 `wezterm-mux-server`**，
#    于是你读到的是一个全新默认 shell 的 pane（假绿/假红都可能是它）。
#    正确做法：GUI 启动后取 `/run/user/$UID/wezterm/gui-sock-<pid>`，
#    用 `WEZTERM_UNIX_SOCKET=<该 socket>` 跑 cli。
#
# 2. **没有鼠标追踪的查询面**。tmux 有 `#{mouse_any_flag}`、PTY 矩阵能扫
#    私有模式序列，但 WezTerm 的 cli 不暴露「应用有没有申请鼠标」。
#    所以本脚本**不断言**鼠标维度（该项由 PTY 矩阵 + tmux 覆盖），
#    不假装测过。
#
# 用法：scripts/e2e-v2-wezterm.sh
# 前置：qaqh-daemon 与 qaqh-tui 已构建；`wezterm` 在 PATH 上。
# 缺 wezterm / 无可用显示环境时**显式 SKIP**（exit 0），不假装通过。
# 隔离：独立 GUI 进程（`--always-new-process` + 唯一 class）+ 私有
#       QAQH_DATA_DIR，不碰用户正在用的 WezTerm 实例。

set -u

SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
REPO_ROOT=$(cd -- "$SCRIPT_DIR/.." && pwd)
BACKEND_ROOT=${QAQH_BACKEND_ROOT:-$REPO_ROOT/../qaqh-backend-anchor}
DAEMON=${DAEMON:-$BACKEND_ROOT/target/debug/qaqh-daemon}
TUI=${TUI:-$REPO_ROOT/target/debug/qaqh-tui}
D=${D:-/tmp/qaqh-e2e-v2-wezterm}
# 唯一 class：用来把「我起的那个 GUI」从用户自己的 WezTerm 里认出来。
WEZ_CLASS=${WEZ_CLASS:-qaqh-e2e-$$}

for binary in "$DAEMON" "$TUI"; do
    [ -x "$binary" ] || {
        echo "缺少可执行文件：$binary（先 cargo build）" >&2
        exit 1
    }
done

if ! command -v wezterm >/dev/null 2>&1; then
    echo "SKIP: 未安装 wezterm —— WezTerm 行需要真实终端模拟器"
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
    # python 把 GUI pid 写在文件里（它才知道自己起的是哪一个实例）。
    # WezTerm 被 kill 后**不会**自己清掉 `gui-sock-<pid>`，留着会让后续
    # `wezterm cli` 的连接发现多出垃圾项，所以这里顺手删掉。
    if [ -f "$D/gui-pid" ]; then
        WEZ_PID=$(cat "$D/gui-pid")
        kill "$WEZ_PID" 2>/dev/null || true
        python3 - "$WEZ_PID" <<'PYEOF' 2>/dev/null || true
import os
import pathlib
import sys

pathlib.Path(
    f"/run/user/{os.getuid()}/wezterm/gui-sock-{sys.argv[1]}"
).unlink(missing_ok=True)
PYEOF
    fi
    kill "$DAEMON_PID" 2>/dev/null || true
    wait "$DAEMON_PID" 2>/dev/null || true
}
trap cleanup EXIT

# 记下 GUI pid 供 cleanup 用（python 会把 pid 写进文件）。
D="$D" TUI="$TUI" WEZ_CLASS="$WEZ_CLASS" python3 - <<'PY'
import json
import os
import pathlib
import subprocess
import time
import uuid
from urllib.request import Request, urlopen

D = pathlib.Path(os.environ["D"])
DATA = D / "qaqh"
TUI = os.environ["TUI"]
WEZ_CLASS = os.environ["WEZ_CLASS"]
DISCOVERY = DATA / "daemon.json"
UID = os.getuid()
WEZ_RUNTIME = pathlib.Path(f"/run/user/{UID}/wezterm")

def run(*args, timeout=30, env=None):
    return subprocess.run(
        args, capture_output=True, text=True, timeout=timeout, env=env
    )

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

# ── 在真实 WezTerm 里跑 TUI ────────────────────────────────────────────
# 横幅先打、再灌 40 行 filler ⇒ 横幅必然滚进 scrollback，读历史才有意义。
# `unset NO_COLOR` 同另两个脚本：ambient NO_COLOR=1 会让整屏 Color::Reset。
BANNER = f"WEZ-BANNER-{uuid.uuid4().hex[:8]}"
wrapper = D / "run-in-wezterm.sh"
wrapper.write_text(
    "#!/bin/bash\n"
    "unset NO_COLOR\n"
    "export COLORTERM=truecolor\n"
    f'export QAQH_DATA_DIR="{DATA}"\n'
    f'echo "{BANNER}"\n'
    'for i in $(seq 1 40); do echo "filler-$i"; done\n'
    f'"{TUI}" --v2-agent --no-spawn\n'
    'echo "TUI_EXIT=$?"\n'
    "sleep 120\n"
)
wrapper.chmod(0o755)

launch_env = {k: v for k, v in os.environ.items() if k != "WEZTERM_UNIX_SOCKET"}
gui = subprocess.Popen(
    [
        "wezterm",
        "start",
        "--always-new-process",
        "--class",
        WEZ_CLASS,
        str(wrapper),
    ],
    stdin=subprocess.DEVNULL,
    stdout=subprocess.DEVNULL,
    stderr=subprocess.DEVNULL,
    env=launch_env,
    start_new_session=True,
)

# 找「我这个 class 的 GUI」的 pid → 直连它的 `gui-sock-<pid>`。
# 这一步是必须的：`wezterm cli --class X` 在本版本不会去找 GUI 实例，
# 而是连默认 socket，没有就 spawn 一个 mux-server（读到的是别的 pane）。
gui_pid = None
deadline = time.monotonic() + 25
while time.monotonic() < deadline and gui_pid is None:
    found = run("pgrep", "-f", f"wezterm-gui start --always-new-process --class {WEZ_CLASS}")
    for line in found.stdout.split():
        pid = int(line)
        if (WEZ_RUNTIME / f"gui-sock-{pid}").exists():
            gui_pid = pid
            break
    time.sleep(0.5)

if gui_pid is None:
    print("SKIP: WezTerm GUI 起不来（多半是没有可用显示环境）")
    raise SystemExit(0)

WEZ_SOCK = WEZ_RUNTIME / f"gui-sock-{gui_pid}"
(D / "gui-pid").write_text(str(gui_pid))
print(f"wezterm gui pid={gui_pid} sock={WEZ_SOCK}")

cli_env = dict(launch_env)
cli_env["WEZTERM_UNIX_SOCKET"] = str(WEZ_SOCK)

def cli(*args, timeout=30):
    return run("wezterm", "cli", *args, env=cli_env, timeout=timeout)

def pane_id():
    out = cli("list").stdout
    for line in out.splitlines()[1:]:
        parts = line.split()
        if len(parts) >= 4:
            return int(parts[2])
    return None

def pane_size(pid):
    out = cli("list").stdout
    for line in out.splitlines()[1:]:
        parts = line.split()
        if len(parts) >= 5 and parts[2] == str(pid):
            return parts[4]
    return None

def pane_text(pid, start_line=None, escapes=False):
    args = ["get-text", "--pane-id", str(pid)]
    if start_line is not None:
        args += ["--start-line", str(start_line)]
    if escapes:
        args.append("--escapes")
    return cli(*args).stdout

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

pid = None
deadline = time.monotonic() + 25
while time.monotonic() < deadline and pid is None:
    pid = pane_id()
    if pid is None:
        time.sleep(0.5)
if pid is None:
    print("SKIP: 拿不到 WezTerm pane（GUI 没起来？）")
    raise SystemExit(0)

# 0) scrollback：横幅必须能从历史读回（`--start-line -1000` 往回到 scrollback），
#    且**不在**默认视口（`--start-line` 省略 = 屏幕首行起）里。
def banner_in_history():
    return pid is not None and BANNER in pane_text(pid, start_line=-1000)

if not wait_for(banner_in_history):
    check("scrollback：横幅进入 WezTerm scrollback", False, f"banner={BANNER} pid={pid}")
else:
    in_history = BANNER in pane_text(pid, start_line=-1000)
    visible = BANNER in pane_text(pid)
    check(
        "scrollback：横幅已滚出视口但仍在历史里",
        in_history and not visible,
        f"history={in_history} visible={visible}",
    )
    (D / "pane-history.txt").write_text(pane_text(pid, start_line=-1000))

# 1) 启动渲染：Ctrl+L 打开会话列表 → Enter 打开会话 → 应有 composer / 状态行
time.sleep(3.0)
cli("send-text", "--pane-id", str(pid), "--no-paste", "\x0c")  # Ctrl+L
time.sleep(2.0)
cli("send-text", "--pane-id", str(pid), "--no-paste", "\r")  # Enter
wait_for(lambda: "❯" in pane_text(pid))
time.sleep(1.0)
text = pane_text(pid)
(D / "pane.plain.txt").write_text(text)
check("启动渲染：composer 提示符可见", "❯" in text, repr(text[-200:]))
check("启动渲染：状态行 ready 可见", "ready" in text, "")
check("启动渲染：无 panic", "panicked at" not in text, "")

# 2) truecolor：WezTerm 回读的 SGR 里应有 24-bit 颜色
ansi = pane_text(pid, escapes=True)
(D / "pane.ansi.txt").write_text(ansi)
check(
    "truecolor：回读含 24-bit SGR",
    any(marker in ansi for marker in ("38;2;", "48;2;", "38:2:", "48:2:")),
    "（读回里没有任何 24-bit SGR 说明 WezTerm 侧没拿到真彩色）",
)

# 3) resize：`split-pane` 把 TUI 所在 pane 挤窄 → 真实几何变化 + SIGWINCH。
#    （WezTerm cli 没有「改窗口尺寸」的命令；分屏是它唯一能程序化改 pane 几何的路径。）
before_size = pane_size(pid)
split = cli("split-pane", "--right", "--percent", "50")
time.sleep(2.5)
after_size = pane_size(pid)
check(
    "resize：TUI pane 几何已变（split-pane → SIGWINCH）",
    before_size != after_size and after_size is not None,
    f"{before_size} -> {after_size} rc={split.returncode}",
)
text_after = pane_text(pid)
check("resize：重排后 UI 仍在（composer 可见）", "❯" in text_after, "")
check("resize：重排后无 panic", "panicked at" not in text_after, "")

# 4) 退出：Ctrl+Q → 退出码回显 0。
#    注意 0x11 在**行规程**里是 XON，用 `cat` 之类的普通进程验证会看不到它；
#    TUI 处于 raw 模式（IXON 关）才收得到，所以这里直接对 TUI 发。
cli("send-text", "--pane-id", str(pid), "--no-paste", "\x11")
exit_text = ""
deadline = time.monotonic() + 20
while time.monotonic() < deadline:
    exit_text = pane_text(pid, start_line=-1000)
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
