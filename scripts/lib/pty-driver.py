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
        [--key <秒>:<两位十六进制字节>]... [--quit] [--exit-code-file <路径>]

环境变量**原样继承**（调用方在 shell 里设好 `QAQH_DATA_DIR` 等即可）；
`TUI_ARGS` 与 smoke 同款，按 shell 分词后追加到命令行。

- `--key 30:12`：第 30 秒往 pty 写一个字节 `0x12`（Ctrl+R）。可重复，按时间排序。
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
    """`30:12` → (30.0, b'\\x12')。"""
    try:
        at, hex_byte = spec.split(":", 1)
        value = bytes([int(hex_byte, 16)])
    except ValueError as exc:  # noqa: TRY003 - CLI 用法错误，直接给原文更清楚
        raise SystemExit(f"--key 需要 <秒>:<两位十六进制>，收到 {spec!r}") from exc
    if len(value) != 1:
        raise SystemExit(f"--key 的字节必须恰好 1 字节，收到 {spec!r}")
    return float(at), value


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--tui", required=True)
    parser.add_argument("--raw", required=True)
    parser.add_argument("--seconds", type=float, required=True)
    parser.add_argument("--rows", type=int, default=40)
    parser.add_argument("--cols", type=int, default=130)
    parser.add_argument("--key", action="append", default=[])
    parser.add_argument("--quit", action="store_true")
    parser.add_argument("--exit-code-file")
    args = parser.parse_args()

    keys = sorted((parse_key(spec) for spec in args.key), key=lambda item: item[0])
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
