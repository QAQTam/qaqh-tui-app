#!/usr/bin/env python3
"""真 PTY 驱动：跑 TUI 一段固定时长，把原始输出落盘。

## 为什么需要它

v2 Agent View 初始化 inline viewport 时会发 `ESC[6n`（DSR / 光标位置查询），
**等终端回答**。`script(1)` 与 `pty.spawn` 这类"哑"驱动不解析输出、也不回话，
于是查询超时，TUI 在初始化阶段直接 panic：

    failed to initialize terminal: The cursor position could not be read
    within a normal duration

这条 panic 是**驱动的锅，不是产品的锅**——真终端（xterm/kitty/tmux/…）都会回
`ESC[row;colR`。alpha1 把默认 UI 从 v1 全屏切到 v2 Agent View 之后，所有仍在用
哑驱动的 harness 会**静默变成假红**（渲染都没跑起来，相位解析自然全空）。

`scripts/smoke-tui.sh` 早就踩过这个坑并在脚本内联了修法；本文件把那套逻辑抽成
公共件，给 `e2e-lease-expiry.sh` / `e2e-restart.sh` / `e2e-session-list.sh` 复用，
避免每个脚本各自复制一份、又各自陈旧一次。

## 用法

    scripts/lib/pty-driver.py --tui <TUI 路径> --raw <原始输出> --seconds <N> \
        [--key <秒>:<两位十六进制字节>]... [--type <秒>:<文本>]... \
        [--quit] [--exit-code-file <路径>]

环境变量**原样继承**（调用方在 shell 里设好 `QAQH_DATA_DIR` 等即可）；
`TUI_ARGS` 与 smoke 同款，按 shell 分词后追加到命令行。

- `--key 30:12`：第 30 秒往 pty 写字节 `0x12`（Ctrl+R）。可重复，按时间排序；
  参数是**十六进制字节串**，所以多字节序列也能直接发，例如 SGR 鼠标：
  `--key 9:1b5b3c303b32363b31304d`（`ESC[<0;26;10M` = 左键在第 10 行第 26 列按下）。
- `--type 8:hello`：第 8 秒把 `hello` 的 UTF-8 字节原样写进去（模拟打字）；
  回车另给 `--key 8.5:0d`。
- `--quit`：到 `--seconds` 时先发 Ctrl+Q（干净退出路径），再等 3 秒。
- 无论走哪条路，最后都按进程组收尸（SIGTERM → SIGKILL），不留孤儿。
- `--exit-code-file`：把 TUI 退出码写进去（`smoke-tui.sh` 的判据③需要它）。
"""

import argparse
import fcntl
import os
import pathlib
import pty
import select
import shlex
import signal
import struct
import subprocess
import sys
import termios
import time

DSR_QUERY = b"\x1b[6n"
DSR_REPLY = b"\x1b[1;1R"


def parse_key(spec: str) -> tuple[float, bytes]:
    """`30:12` → (30.0, b'\\x12')；也接受多字节十六进制串（SGR 鼠标等）。

    多字节是给鼠标用的：一次按下就是 `ESC [ < 0 ; 列 ; 行 M` 一整串，
    拆成单字节按键再拼时间表既难写又容易错位。例：

        1b5b3c303b32363b31304d   # ESC[<0;26;10M —— SGR 左键按下
    """
    try:
        at, hex_bytes = spec.split(":", 1)
        value = bytes.fromhex(hex_bytes)
    except ValueError as exc:  # noqa: TRY003 - CLI 用法错误，直接给原文更清楚
        raise SystemExit(f"--key 需要 <秒>:<十六进制字节串>，收到 {spec!r}") from exc
    if not value:
        raise SystemExit(f"--key 的字节串不能为空，收到 {spec!r}")
    return float(at), value


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--tui", required=True)
    parser.add_argument("--raw", required=True)
    parser.add_argument("--seconds", type=float, required=True)
    parser.add_argument("--rows", type=int, default=40)
    parser.add_argument("--cols", type=int, default=130)
    parser.add_argument("--key", action="append", default=[])
    # `--type 8:hello` → 第 8 秒把 "hello" 的 UTF-8 字节按原样写进 pty（模拟用户
    # 在 composer 里打字）。回车等控制字节用 `--key`，例如 `--key 8.5:0d`。
    parser.add_argument("--type", action="append", default=[])
    # `--respond 工具权限:61` → 输出里出现 `工具权限` 时回一个字节 `0x61`（'a'）。
    # 用于"界面出现某个弹窗就应答"的场景（如权限 modal 出现就批准）。只回一次。
    # ⚠ needle 必须是在**字节流里连续出现**的串：TUI 按光标定位分段写，
    # 跨段的长句（如英文说明）会搜不到 —— 用短的、成段写出的标记（如标题）。
    parser.add_argument("--respond", action="append", default=[])
    parser.add_argument("--quit", action="store_true")
    parser.add_argument("--exit-code-file")
    args = parser.parse_args()

    keys = [parse_key(spec) for spec in args.key]
    for spec in args.type:
        at, _, text = spec.partition(":")
        if not text:
            raise SystemExit(f"--type 需要 <秒>:<文本>，收到 {spec!r}")
        keys.append((float(at), text.encode()))
    keys.sort(key=lambda item: item[0])
    # (needle 字节, 回什么字节, 是否已回过)
    responds: list[list[object]] = []
    for spec in args.respond:
        needle, _, hex_byte = spec.partition(":")
        if not needle or not hex_byte:
            raise SystemExit(f"--respond 需要 <标记>:<两位十六进制>，收到 {spec!r}")
        responds.append([needle.encode(), bytes([int(hex_byte, 16)]), False])
    raw_path = pathlib.Path(args.raw)

    master, slave = pty.openpty()
    fcntl.ioctl(
        slave,
        termios.TIOCSWINSZ,
        struct.pack("HHHH", args.rows, args.cols, 0, 0),
    )
    env = os.environ.copy()
    env["TERM"] = "xterm-256color"
    tui = subprocess.Popen(
        [args.tui, *shlex.split(env.get("TUI_ARGS", ""))],
        stdin=slave,
        stdout=slave,
        stderr=slave,
        env=env,
        start_new_session=True,
        close_fds=True,
    )
    os.close(slave)
    os.set_blocking(master, False)

    capture = bytearray()
    # 只保留尾部窗口：`ESC[6n` 可能被切在两次 read 之间，留一点重叠避免漏答。
    query_tail = bytearray()
    # 与 DSR 用的 `query_tail` 分开：那个会被"就地消费"，respond 需要看完整历史。
    respond_tail = bytearray()
    start = time.monotonic()
    pending = list(keys)
    quit_at = args.seconds
    quit_sent = False
    # Ctrl+Q 之后留 3 秒让它走完收尾（smoke 同款）。
    deadline = args.seconds + 3

    while time.monotonic() - start < deadline:
        elapsed = time.monotonic() - start

        while pending and elapsed >= pending[0][0]:
            _, byte = pending.pop(0)
            try:
                os.write(master, byte)
            except OSError:
                pass

        if args.quit and not quit_sent and elapsed >= quit_at:
            quit_sent = True
            try:
                os.write(master, b"\x11")  # Ctrl+Q
            except OSError:
                pass

        ready, _, _ = select.select([master], [], [], 0.05)
        if ready:
            try:
                chunk = os.read(master, 65536)
            except (BlockingIOError, OSError):
                chunk = b""
            if chunk:
                capture.extend(chunk)
                query_tail.extend(chunk)
                while DSR_QUERY in query_tail:
                    index = query_tail.index(DSR_QUERY)
                    del query_tail[: index + len(DSR_QUERY)]
                    try:
                        os.write(master, DSR_REPLY)
                    except OSError:
                        pass
                # 兜底：极长的非 DSR 输出不该无限增长。
                if len(query_tail) > 1 << 16:
                    del query_tail[: len(query_tail) - len(DSR_QUERY)]
                respond_tail.extend(chunk)
                if len(respond_tail) > 1 << 16:
                    del respond_tail[: len(respond_tail) - (1 << 15)]
                for rule in responds:
                    if not rule[2] and rule[0] in respond_tail:
                        rule[2] = True
                        try:
                            os.write(master, rule[1])
                        except OSError:
                            pass

        if tui.poll() is not None:
            break

    if tui.poll() is None:
        try:
            os.killpg(tui.pid, signal.SIGTERM)
        except ProcessLookupError:
            pass
        try:
            tui.wait(timeout=2)
        except subprocess.TimeoutExpired:
            try:
                os.killpg(tui.pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
            tui.wait(timeout=2)
    os.close(master)

    raw_path.write_bytes(capture)
    if args.exit_code_file:
        pathlib.Path(args.exit_code_file).write_text(str(tui.returncode), encoding="utf-8")
    return 0


if __name__ == "__main__":
    sys.exit(main())
