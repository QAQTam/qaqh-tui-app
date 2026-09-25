#!/bin/bash
# 真机端到端：V2 Agent View 在**真实终端模拟器**里的链路（M6.3 / 兼容矩阵 §3）。
#
# 与 `e2e-v2-terminal-matrix.sh` 的区别：那边是「自己开 PTY + 解析字节流」，
# 验证的是环境能力组合；这里把 TUI 放进**真正的终端模拟器**（kitty），用
# `kitten @ get-text` 读回**模拟器解出来的屏幕**，因此能验证只有真模拟器才
# 能回答的问题：
#
#   render    启动后屏幕里真的有 UI（composer / 状态行 / tab），不是空白；
#   truecolor 模拟器解出的 SGR 里带 24-bit 颜色（`38;2;` / `48;2;`）；
#   resize    改**OS 窗口**尺寸（真实 SIGWINCH 路径）后 UI 跟着重排且不崩；
#   scrollback `--extent=all` 能读回 TUI 之前写进 scrollback 的行；
#   exit      Ctrl+Q 后退出码为 0，且终端状态被还原（回到 shell）。
#
# 判据全部来自模拟器的屏幕回读，不看 TUI 自述。
#
# 用法：scripts/e2e-v2-real-terminal.sh
# 前置：qaqh-daemon 与 qaqh-tui 已构建；`kitty` 与 `kitten` 在 PATH 上。
# 缺 kitty / 无可用显示环境时**显式 SKIP**（exit 0），不假装通过。
# 隔离：私有 QAQH_DATA_DIR（/tmp 下），不碰正在运行的 daemon 与会话。

set -u

SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
REPO_ROOT=$(cd -- "$SCRIPT_DIR/.." && pwd)
BACKEND_ROOT=${QAQH_BACKEND_ROOT:-$REPO_ROOT/../qaqh-backend}
DAEMON=${DAEMON:-$BACKEND_ROOT/target/debug/qaqh-daemon}
TUI=${TUI:-$REPO_ROOT/target/debug/qaqh-tui}
D=${D:-/tmp/qaqh-e2e-v2-real-terminal}
SOCK=${SOCK:-/tmp/qaqh-e2e-v2-real-terminal.sock}

for binary in "$DAEMON" "$TUI"; do
    [ -x "$binary" ] || {
        echo "缺少可执行文件：$binary（先 cargo build）" >&2
        exit 1
    }
done

if ! command -v kitty >/dev/null 2>&1 || ! command -v kitten >/dev/null 2>&1; then
    echo "SKIP: 未安装 kitty/kitten —— 真实终端模拟器矩阵需要模拟器本身"
    exit 0
fi

case "$D" in
    /tmp/*) ;;
    *)
        echo "D 必须位于 /tmp 下：$D" >&2
        exit 1
        ;;
esac

python3 - "$D" "$SOCK" <<'PY'
import pathlib
import shutil
import sys

d = pathlib.Path(sys.argv[1])
sock = pathlib.Path(sys.argv[2])
if d.exists():
    shutil.rmtree(d)
(d / "qaqh").mkdir(parents=True)
sock.unlink(missing_ok=True)
PY

# ── 隔离 daemon（setsid：跨命令存活，脚本结束自己收）────────────────────
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
    # 关掉探测窗口（若还在），再收 daemon。
    timeout 10 kitten @ --to "unix:$SOCK" close-window --match all >/dev/null 2>&1 || true
    kill "$DAEMON_PID" 2>/dev/null || true
    wait "$DAEMON_PID" 2>/dev/null || true
}
trap cleanup EXIT

D="$D" SOCK="$SOCK" DAEMON="$DAEMON" TUI="$TUI" python3 - <<'PY'
import json
import os
import pathlib
import subprocess
import time
import uuid
from urllib.request import Request, urlopen

D = pathlib.Path(os.environ["D"])
DATA = D / "qaqh"
SOCK = os.environ["SOCK"]
TUI = os.environ["TUI"]
DISCOVERY = DATA / "daemon.json"

def run(*args, **kwargs):
    return subprocess.run(args, capture_output=True, text=True, **kwargs)

def post_json(url, payload, headers=None):
    body = json.dumps(payload).encode()
    request = Request(url, data=body, method="POST")
    request.add_header("Content-Type", "application/json")
    for key, value in (headers or {}).items():
        request.add_header(key, value)
    with urlopen(request, timeout=10) as response:
        return json.loads(response.read())

def kitten(*args, timeout=20):
    return run("kitten", "@", "--to", f"unix:{SOCK}", *args, timeout=timeout)

def window_id():
    out = kitten("ls").stdout
    try:
        listing = json.loads(out)
    except json.JSONDecodeError:
        return None
    for os_window in listing:
        for tab in os_window.get("tabs", []):
            for window in tab.get("windows", []):
                return window.get("id")
    return None

def screen(extent="screen", ansi=False, match=None):
    args = ["get-text", "--match", f"id:{match}", f"--extent={extent}"]
    if ansi:
        args = ["get-text", "--match", f"id:{match}", "--ansi=yes", f"--extent={extent}"]
    return kitten(*args).stdout

# ── 隔离 provider 配置：本脚本不开回合（#42 未修，转写内容不可断言），
#    但 session_create 需要一个可解析的 provider 配置。
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

# ── 在真实模拟器里跑 TUI ────────────────────────────────────────────────
# wrapper 分两段：
#   ① scrollback 段：先灌 40 行 filler，让**横幅滚出屏幕**，`touch READY` 后
#      等 python 放行——python 借这个握手断言「滚出屏幕的行仍能从 scrollback
#      读回」，而不是只读还在屏幕上的行；
#   ② TUI 段：跑 TUI，退出后把退出码打回屏幕（用它断言干净退出 + 终端状态还原）。
#
# `unset NO_COLOR` 是必须的：本仓 CI/开发环境常见 `NO_COLOR=1`，那会让
# `ColorSupport::detect()` 判成 NoColor，整屏 `Color::Reset`，truecolor 断言
# 必然假红。这里显式声明这个 profile 要真彩色。
BANNER = f"REALTERM-BANNER-{uuid.uuid4().hex[:8]}"
READY = D / "ready"
GO = D / "go"
wrapper = D / "run-in-kitty.sh"
wrapper.write_text(
    "#!/bin/bash\n"
    "unset NO_COLOR\n"
    "export TERM=xterm-kitty\n"
    "export COLORTERM=truecolor\n"
    f'export QAQH_DATA_DIR="{DATA}"\n'
    # 横幅**先打**，后面再灌 40 行 filler：这样横幅必然滚出屏幕，scrollback
    # 断言才有意义（横幅放最后只会留在屏幕上，测的是屏幕不是历史）。
    f'echo "{BANNER}"\n'
    'for i in $(seq 1 40); do echo "filler-$i"; done\n'
    f'touch "{READY}"\n'
    f'while [ ! -f "{GO}" ]; do sleep 0.1; done\n'
    f'"{TUI}" --no-spawn\n'
    'echo "TUI_EXIT=$?"\n'
    "sleep 120\n"
)
wrapper.chmod(0o755)

launched = run(
    "kitty",
    f"--listen-on=unix:{SOCK}",
    "-o",
    "allow_remote_control=socket-only",
    "--detach",
    "--title",
    "qaqh-real-terminal",
    str(wrapper),
)
if launched.returncode != 0:
    print("SKIP: kitty 起不来（多半是没有可用显示环境）")
    print(launched.stderr.strip()[:400])
    raise SystemExit(0)

wid = None
deadline = time.monotonic() + 20
while time.monotonic() < deadline and wid is None:
    wid = window_id()
    time.sleep(0.5)
if wid is None:
    print("SKIP: kitty 进程起了但拿不到窗口（无显示环境？）")
    raise SystemExit(0)
print(f"kitty window id={wid}")

checks = []
def check(name, passed, detail=""):
    checks.append((name, bool(passed), detail))
    return passed

def geometry():
    try:
        for os_window in json.loads(kitten("ls").stdout):
            for tab in os_window.get("tabs", []):
                for window in tab.get("windows", []):
                    if window.get("id") == wid:
                        return (window.get("columns"), window.get("lines"))
    except json.JSONDecodeError:
        pass
    return None

# 0) scrollback：横幅滚出屏幕后仍要能从 `--extent=all` 读回。
#    这是「模拟器真的把历史留在 scrollback 里」的证据，不是「屏幕上还看得见」。
deadline = time.monotonic() + 20
while time.monotonic() < deadline and not READY.exists():
    time.sleep(0.2)
if not READY.exists():
    check("scrollback：wrapper 未到达 READY 握手点", False, "")
else:
    all_text = screen(extent="all", match=wid)
    on_screen = screen(extent="screen", match=wid)
    check(
        "scrollback：滚出屏幕的行仍能从 --extent=all 读回",
        BANNER in all_text and BANNER not in on_screen,
        f"all_has={BANNER in all_text} screen_has={BANNER in on_screen}",
    )
    (D / "scrollback-all.txt").write_text(all_text)
GO.touch()

# 1) 启动渲染：Ctrl+L 打开会话列表 → Enter 打开会话 → 应有 composer / 状态行
time.sleep(4.0)
kitten("send-key", "--match", f"id:{wid}", "ctrl+l")
time.sleep(2.0)
kitten("send-key", "--match", f"id:{wid}", "enter")
time.sleep(3.0)
text = screen(match=wid)
check("启动渲染：composer 提示符可见", "❯" in text, repr(text[-200:]))
check("启动渲染：状态行 ready 可见", "ready" in text, "")
check("启动渲染：无 panic", "panicked at" not in text, "")

# 2) truecolor：模拟器解出的 SGR 里应有 24-bit 颜色。
#    注意 kitty 用 **T.416 冒号形式**（`38:2:r:g:b`），不是分号形式
#    （`38;2;r;g;b`）——两种都算真彩色，别只认一种（实测踩过）。
ansi = screen(ansi=True, match=wid)
(D / "screen.ansi.txt").write_text(ansi)
(D / "screen.plain.txt").write_text(text)
check(
    "truecolor：屏幕回读含 24-bit SGR",
    any(marker in ansi for marker in ("38;2;", "48;2;", "38:2:", "48:2:")),
    "（读回里没有任何 24-bit SGR 说明模拟器侧没拿到真彩色）",
)

# 3) resize：改**真实终端几何** → UI 必须跟着重排且不崩。
#
# 为什么用改字号而不是 `resize-os-window`：本机 compositor（COSMIC/Wayland）
# 下 `resize-os-window` 返回 0 但**窗口尺寸不变**（实测 142x32 → 142x32），
# 拿它当断言等于假绿。改字号会真改 cell 网格并触发 SIGWINCH，是同一类
# 「终端几何变化」路径。
#
# 字号取 28（实测网格约 57 列）：状态行 ~70 字符，窄于它必然换行/截断，
# 所以「屏幕内容确实变了」这条断言测的是**真的重排**而不是空转。
before_geo = geometry()
kitten("set-font-size", "28")
time.sleep(2.5)
after_geo = geometry()
check(
    "resize：真实终端几何已变（改字号 → SIGWINCH）",
    before_geo != after_geo and after_geo is not None,
    f"{before_geo} -> {after_geo}",
)
text_after = screen(match=wid)
check("resize：重排后 UI 仍在（composer 可见）", "❯" in text_after, "")
check("resize：重排后无 panic", "panicked at" not in text_after, "")
check("resize：前后屏幕内容确实变了", text != text_after, "")

# 4) 退出：Ctrl+Q → 退出码回显 0（说明终端状态已还原、shell 拿回控制权）
kitten("send-key", "--match", f"id:{wid}", "ctrl+q")
exit_text = ""
deadline = time.monotonic() + 20
while time.monotonic() < deadline:
    exit_text = screen(match=wid)
    if "TUI_EXIT=" in exit_text:
        break
    time.sleep(0.5)
check("退出：Ctrl+Q 后回显 TUI_EXIT=0", "TUI_EXIT=0" in exit_text, repr(exit_text[-160:]))

ok = True
for name, passed, detail in checks:
    ok &= passed
    suffix = f"  {detail}" if (detail and not passed) else ""
    print(f"  [{'✓' if passed else '✗'}] {name}{suffix}")
print("RESULT:", "PASS" if ok else "FAIL")
raise SystemExit(0 if ok else 1)
PY
