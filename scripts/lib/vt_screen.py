#!/usr/bin/env python3
"""极简 VT 屏幕重建：把 TUI 的**差分流**还原成最终画面。

## 为什么不能只"去 ANSI 再 grep"

TUI 不是逐行打印，而是按光标定位**只写变化的单元格**。同一行的两段文字之间
只要隔了没变的空格，那些空格就**不会出现在字节流里**：

    原始:  ESC[4;6H Bun ESC[4;10H 引导 ESC[4;15H daemon
    去ANSI: "Bun引导daemon"          ← 空格没了

于是"标题是 `Bun 引导 daemon`"这类断言会**假红**（实测：e2e-session-list 的
A 用例）。相位 token（`● ready`）恰好每次都被连续写出，所以旧写法侥幸能用；
一旦渲染器改成按段写就会跟着坏。凡是要断言"屏幕上显示了什么"的地方，都应该
先重建屏幕，而不是在差分流上 grep。

## 覆盖范围

只实现本仓 TUI 实际用到的子集，刻意不做完整 VT：

- `CSI n;m H` / `f`（光标定位，1-based）
- `CSI n J`（2=清屏）/ `CSI n K`（0=清到行尾）
- `CSI n A/B/C/D`（相对移动，TUI 重绘偶尔用）
- `CSI ... m`（SGR，忽略）
- `\\r` / `\\n` / `\\b`
- 宽字符（CJK）占两列，与终端一致

其他序列一律忽略——**不要**因为某条没处理就以为屏幕重建错了，先看是不是本
模块的范围外。

## 用法

    scripts/lib/vt_screen.py <raw 文件>            # 打印重建后的屏幕
    from vt_screen import render_text              # 或当模块用
"""

import pathlib
import re
import sys
import unicodedata

CSI = re.compile(r"\x1b\[([0-9;?]*)([a-zA-Z])")
# OSC（如设置标题）：`ESC ] ... BEL` 或 `ESC ] ... ESC \`
OSC = re.compile(r"\x1b\][^\x07\x1b]*(?:\x07|\x1b\\)")


def char_width(ch: str) -> int:
    if unicodedata.combining(ch):
        return 0
    return 2 if unicodedata.east_asian_width(ch) in ("W", "F") else 1


class Screen:
    def __init__(self, rows: int, cols: int) -> None:
        self.rows = rows
        self.cols = cols
        self.grid: list[list[str]] = [[" "] * cols for _ in range(rows)]
        self.row = 0
        self.col = 0

    def _ensure(self, row: int, col: int) -> None:
        # 终端不会因为写到底部就报错；行数超出时向下扩，最后再裁掉空行。
        while row >= len(self.grid):
            self.grid.append([" "] * self.cols)
        if col >= self.cols:
            # 行内超宽（TUI 按窗口宽排版，正常不会发生）：就地扩列而不是丢弃。
            for line in self.grid:
                line.extend([" "] * (col - self.cols + 1))
            self.cols = col + 1

    def move(self, row: int, col: int) -> None:
        self._ensure(max(row, 0), max(col, 0))
        self.row = max(row, 0)
        self.col = max(col, 0)

    def write(self, text: str) -> None:
        for ch in text:
            if ch == "\r":
                self.col = 0
                continue
            if ch == "\n":
                self.row += 1
                self.col = 0
                self._ensure(self.row, self.col)
                continue
            if ch == "\b":
                self.col = max(0, self.col - 1)
                continue
            width = char_width(ch)
            if width == 0:
                # 组合字符：贴到前一格。
                if self.col > 0:
                    self.grid[self.row][self.col - 1] += ch
                continue
            self._ensure(self.row, self.col + width)
            self.grid[self.row][self.col] = ch
            if width == 2 and self.col + 1 < self.cols:
                self.grid[self.row][self.col + 1] = ""
            self.col += width

    def erase_line_to_end(self) -> None:
        self._ensure(self.row, self.col)
        for col in range(self.col, self.cols):
            self.grid[self.row][col] = " "

    def erase_display(self) -> None:
        self.grid = [[" "] * self.cols for _ in range(self.rows)]
        self.row = 0
        self.col = 0

    def text(self) -> str:
        lines = []
        for line in self.grid:
            # 宽字符的占位格是 ""，直接拼接不会多出空格。
            lines.append("".join(line).rstrip())
        while lines and not lines[-1]:
            lines.pop()
        return "\n".join(lines)


def render_text(raw: str, rows: int = 60, cols: int = 200) -> str:
    screen = Screen(rows, cols)
    pos = 0
    while True:
        match = CSI.search(raw, pos)
        osc = OSC.search(raw, pos)
        # 先处理 OSC：它可能包着看起来像 CSI 的内容。
        if osc and (not match or osc.start() < match.start()):
            screen.write(raw[pos : osc.start()])
            pos = osc.end()
            continue
        if not match:
            screen.write(raw[pos:])
            break
        screen.write(raw[pos : match.start()])
        params, final = match.group(1), match.group(2)
        numbers = [int(p) if p.isdigit() else 0 for p in params.split(";") if p != ""] or [0]
        if final in ("H", "f"):
            row = (numbers[0] if len(numbers) > 0 and numbers[0] else 1) - 1
            col = (numbers[1] if len(numbers) > 1 and numbers[1] else 1) - 1
            screen.move(row, col)
        elif final == "J":
            if numbers[0] in (2, 3):
                screen.erase_display()
        elif final == "K":
            if numbers[0] == 0:
                screen.erase_line_to_end()
        elif final == "A":
            screen.move(screen.row - max(1, numbers[0]), screen.col)
        elif final == "B":
            screen.move(screen.row + max(1, numbers[0]), screen.col)
        elif final == "C":
            screen.move(screen.row, screen.col + max(1, numbers[0]))
        elif final == "D":
            screen.move(screen.row, screen.col - max(1, numbers[0]))
        # 其余（SGR `m`、DEC 私有 `?25l` 等）忽略。
        pos = match.end()
    return screen.text()


def main() -> int:
    if len(sys.argv) < 2:
        print(__doc__.split("## 用法")[-1].strip(), file=sys.stderr)
        return 2
    raw = pathlib.Path(sys.argv[1]).read_text(errors="replace")
    print(render_text(raw))
    return 0


if __name__ == "__main__":
    sys.exit(main())
