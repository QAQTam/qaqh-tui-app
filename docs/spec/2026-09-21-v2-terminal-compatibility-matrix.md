# QAQH TUI v2 终端兼容矩阵

> 日期：2026-09-21
> 状态：环境能力矩阵已自动化；真实终端模拟器矩阵进行中

## 1. 自动化入口

```bash
scripts/e2e-v2-terminal-matrix.sh
```

该脚本在隔离 daemon + 独立 PTY 中依次运行以下 profile，并断言：

- TUI 退出码为 0；
- 无 panic；
- 无 cursor-position timeout。

## 2. 环境能力矩阵

| Profile | TERM / 环境 | 主题 | 结果 | 说明 |
|---|---|---|---|---|
| night-16 | `TERM=xterm` | Night | PASS | 16 色降级 |
| night-256 | `TERM=xterm-256color` | Night | PASS | 256 色 |
| night-truecolor | `COLORTERM=truecolor` | Night | PASS | truecolor |
| day-256 | `TERM=xterm-256color` | Day | PASS | 亮色主题 |
| terminal-16 | `TERM=xterm` | Terminal | PASS | 终端原生 16 色 |
| no-color | `NO_COLOR=1` | 默认 | PASS | 无颜色输出 |
| term-dumb | `TERM=dumb` | 默认 | PASS | 基础终端降级 |
| tmux-256 | `TERM=tmux-256color` + `TMUX` | 默认 | PASS | tmux 环境变量分支 |
| screen-256 | `TERM=screen-256color` | 默认 | PASS | screen 兼容 TERM |
| kitty | `TERM=xterm-kitty` | 默认 | PASS | Kitty 环境分支 |
| alacritty | `TERM=alacritty` | 默认 | PASS | Alacritty 环境分支 |
| ssh-xterm | `TERM=xterm-256color` + `SSH_TTY` | 默认 | PASS | SSH 环境变量分支 |

最近一次实测：

```text
[✓] night-16         exit=0 queries=3 bytes=6289
[✓] night-256        exit=0 queries=3 bytes=6551
[✓] night-truecolor  exit=0 queries=3 bytes=6674
[✓] day-256          exit=0 queries=3 bytes=6322
[✓] terminal-16      exit=0 queries=3 bytes=1884
[✓] no-color         exit=0 queries=3 bytes=1870
[✓] term-dumb        exit=0 queries=3 bytes=2092
[✓] tmux-256         exit=0 queries=3 bytes=6427
[✓] screen-256       exit=0 queries=3 bytes=6427
[✓] kitty            exit=0 queries=3 bytes=6643
[✓] alacritty        exit=0 queries=3 bytes=6668
[✓] ssh-xterm        exit=0 queries=3 bytes=6402
RESULT: PASS
```

## 3. 真实终端模拟器矩阵

以下项目需要对应实机/虚拟机，当前尚未声称通过：

| 终端 | scrollback | inline viewport | resize | truecolor | 鼠标/复制 | 状态 |
|---|---|---|---|---|---|---|
| Windows Terminal / PowerShell | 待测 | 待测 | 待测 | 待测 | 待测 | PENDING |
| WezTerm | 待测 | 待测 | 待测 | 待测 | 待测 | PENDING |
| iTerm2 | 待测 | 待测 | 待测 | 待测 | 待测 | PENDING |
| Alacritty | 待测 | 待测 | 待测 | 待测 | 待测 | PENDING |
| Kitty | 待测 | 待测 | 待测 | 待测 | 待测 | PENDING |
| GNOME Terminal | 待测 | 待测 | 待测 | 待测 | 待测 | PENDING |
| Konsole | 待测 | 待测 | 待测 | 待测 | 待测 | PENDING |
| tmux | 待测 | 待测 | 待测 | 待测 | 待测 | PENDING |
| SSH | 待测 | 待测 | 待测 | 待测 | 待测 | PENDING |

## 4. 已知风险

- Windows ConHost 的 `crossterm::ClearType::Purge` 当前只清可见 screen buffer，
  会话切换后历史清理能力需要 Windows 实机确认；
- `TERM=dumb` 的自动 PTY 已通过初始化/退出，但未验证真实终端滚动和复制行为；
- tmux/SSH 的 cursor query、resize 和 scrollback 行为需要单独记录；
- 主题降级已通过“无崩溃”验证，颜色可读性仍需要人工视觉检查。
