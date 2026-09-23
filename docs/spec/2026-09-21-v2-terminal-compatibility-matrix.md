# QAQH TUI v2 终端兼容矩阵

> 日期：2026-09-21（§3 于 2026-09-23 更新：Kitty 已从「待测」变「已测」）
> 状态：环境能力矩阵已自动化；真实终端模拟器矩阵**已开测**（Kitty ✅，其余待环境）

## 1. 自动化入口

```bash
scripts/e2e-v2-terminal-matrix.sh    # 环境能力矩阵（自开 PTY + 解析字节流）
scripts/e2e-v2-real-terminal.sh      # 真实终端模拟器矩阵（kitty + remote control）
```

前者在隔离 daemon + 独立 PTY 中依次运行下列 profile，并断言：

- TUI 退出码为 0；
- 无 panic；
- 无 cursor-position timeout；
- **未开启鼠标追踪**（`?1000h/1002h/1003h/1006h/1015h` 一个都不出现）——
  这是「鼠标/复制」维度：开了就吃掉终端原生选择/复制。

后者把 TUI 放进**真正的终端模拟器**（kitty），用 `kitten @ get-text` 读回
**模拟器解出来的屏幕**，验证只有真模拟器才能回答的问题（见 §3）。缺
kitty / 无可用显示环境时**显式 SKIP**，不假装通过。

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

最近一次实测（2026-09-23，新增 `mouse` 列）：

```text
[✓] night-16         exit=0 queries=3 bytes=6258 mouse=off
[✓] night-256        exit=0 queries=3 bytes=6402 mouse=off
[✓] night-truecolor  exit=0 queries=3 bytes=6643 mouse=off
[✓] day-256          exit=0 queries=3 bytes=6316 mouse=off
[✓] terminal-16      exit=0 queries=3 bytes=1853 mouse=off
[✓] no-color         exit=0 queries=3 bytes=1864 mouse=off
[✓] term-dumb        exit=0 queries=3 bytes=2061 mouse=off
[✓] tmux-256         exit=0 queries=3 bytes=6450 mouse=off
[✓] screen-256       exit=0 queries=3 bytes=6352 mouse=off
[✓] kitty            exit=0 queries=3 bytes=6643 mouse=off
[✓] alacritty        exit=0 queries=3 bytes=6643 mouse=off
[✓] ssh-xterm        exit=0 queries=3 bytes=6427 mouse=off
RESULT: PASS
```

## 3. 真实终端模拟器矩阵

`scripts/e2e-v2-real-terminal.sh` 已落地，逐维度判据都取自**模拟器的屏幕回读**，
不看 TUI 自述：

| 维度 | 判据 | 说明 |
|---|---|---|
| render | 屏幕里有 `❯`（composer）与 `ready`（状态行），且无 `panicked at` | Ctrl+L → Enter 打开会话后再读 |
| truecolor | 屏幕回读含 24-bit SGR | kitty 用 **T.416 冒号形式** `38:2:r:g:b`，不是分号形式——两种都算 |
| resize | 改字号触发真实 SIGWINCH 后 cell 网格变化 **且屏幕内容确实重排** | 见下方「为什么不是 resize-os-window」 |
| scrollback | 横幅**滚出屏幕**后仍能从 `--extent=all` 读回，且 `--extent=screen` 里没有 | 用 READY/GO 握手保证「先滚出、再断言」 |
| exit | Ctrl+Q 后屏幕回显 `TUI_EXIT=0` | 退出码回显到屏幕 = 终端状态已还原、shell 拿回控制权 |

| 终端 | scrollback | inline viewport | resize | truecolor | 鼠标/复制 | 状态 |
|---|---|---|---|---|---|---|
| **Kitty 0.48.2** | ✅ 已测 | ✅ 已测（render 维度） | ✅ 已测（SIGWINCH 重排） | ✅ 已测（`38:2:`） | ✅ 未开捕获 | **PASS**（2026-09-23） |
| Windows Terminal / PowerShell | 待测 | 待测 | 待测 | 待测 | 待测 | PENDING（无 Windows 实机） |
| WezTerm | 待测 | 待测 | 待测 | 待测 | 待测 | PENDING（环境未安装） |
| iTerm2 | 待测 | 待测 | 待测 | 待测 | 待测 | PENDING（macOS 专属） |
| Alacritty | 待测 | 待测 | 待测 | 待测 | 待测 | PENDING（环境未安装） |
| GNOME Terminal | 待测 | 待测 | 待测 | 待测 | 待测 | PENDING（环境未安装） |
| Konsole | 待测 | 待测 | 待测 | 待测 | 待测 | PENDING（环境未安装） |
| tmux | 待测 | 待测 | 待测 | 待测 | 待测 | PENDING（环境未安装，无 root 装） |
| SSH | 待测 | 待测 | 待测 | 待测 | 待测 | PENDING（无 sshd） |

**鼠标/复制**这一列对 12 个环境能力 profile 也已验证：全部 `mouse=off`。

### 3.1 为什么 resize 用改字号而不是 `resize-os-window`

本机 compositor（COSMIC / Wayland）下 `kitten @ resize-os-window --unit=cells
--width=96 --height=28` **返回 0 但窗口尺寸不变**（实测 `142x32 → 142x32`）。
拿它当断言就是假绿。改字号（`kitten @ set-font-size 28`）会真改 cell 网格
（实测 `142x32 → ~57x13`）并触发 SIGWINCH，属同一类「终端几何变化」路径，
且能测出真实重排（状态行 ~70 字符，窄于它必然换行）。

### 3.2 实测输出（2026-09-23）

```text
[✓] scrollback：滚出屏幕的行仍能从 --extent=all 读回
[✓] 启动渲染：composer 提示符可见
[✓] 启动渲染：状态行 ready 可见
[✓] 启动渲染：无 panic
[✓] truecolor：屏幕回读含 24-bit SGR
[✓] resize：真实终端几何已变（改字号 → SIGWINCH）
[✓] resize：重排后 UI 仍在（composer 可见）
[✓] resize：重排后无 panic
[✓] resize：前后屏幕内容确实变了
[✓] 退出：Ctrl+Q 后回显 TUI_EXIT=0
RESULT: PASS
```

**证伪**：把 wrapper 的 `unset NO_COLOR` 换成 `export NO_COLOR=1` →
`truecolor` 一条立刻变红、`RESULT: FAIL`（已实测后还原）。这条证伪顺带
暴露一个环境陷阱：本机 ambient `NO_COLOR=1`，不显式清掉的话真彩色断言
必然假红。

## 4. 已知风险

- Windows ConHost 的 `crossterm::ClearType::Purge` 当前只清可见 screen buffer，
  会话切换后历史清理能力需要 Windows 实机确认；
- `TERM=dumb` 的自动 PTY 已通过初始化/退出；真实滚动/复制行为仍未在 dumb
  终端上验证（dumb 终端本身无 scrollback 概念，价值有限）；
- tmux/SSH 的 cursor query、resize 和 scrollback 行为需要单独记录
  （环境未安装 tmux/screen、无 sshd，无法本机开测；**不要**拿 `TERM=tmux-*`
  环境变量分支当 tmux 真机证据）；
- 主题降级已通过「无崩溃」验证，颜色可读性仍需要人工视觉检查；
- **本机 compositor 下 `resize-os-window` 是空操作**（返回 0 但尺寸不变），
  所以 §3 的 resize 走改字号路径；真实窗口拖拽缩放仍需在有窗口管理器的
  实机上补一次；
- kitty 是**唯一**已开测的真实模拟器，其余终端在矩阵里保持 PENDING——
  环境能力分支（`TERM=tmux-256color` 等）**不等于**该终端已测。
