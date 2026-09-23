# QAQH TUI v2 终端兼容矩阵

> 日期：2026-09-21（§3 于 2026-09-23 更新：Kitty + tmux 已从「待测」变「已测」）
> 状态：环境能力矩阵已自动化；真实终端模拟器/multiplexer 矩阵**已开测**
> （Kitty ✅、tmux ✅，其余待环境）

## 1. 自动化入口

```bash
scripts/e2e-v2-terminal-matrix.sh    # 环境能力矩阵（自开 PTY + 解析字节流）
scripts/e2e-v2-real-terminal.sh      # 真实终端模拟器（kitty + remote control）
scripts/e2e-v2-tmux.sh               # 真实 multiplexer（tmux，scrollback 语义）
scripts/e2e-v2-wezterm.sh            # 真实终端模拟器（WezTerm cli 回读）
scripts/e2e-v2-alacritty.sh          # 真实终端模拟器（Alacritty，**仅进程级**）
```

第一个在隔离 daemon + 独立 PTY 中依次运行下列 profile，并断言：

- TUI 退出码为 0；
- 无 panic；
- 无 cursor-position timeout；
- **未开启鼠标追踪**（`?1000h/1002h/1003h/1006h/1015h` 一个都不出现）——
  这是「鼠标/复制」维度：开了就吃掉终端原生选择/复制。

后两个分别把 TUI 放进**真正的终端模拟器**（kitty）与**真正的 multiplexer**
（tmux），判据取自对方的回读接口（`kitten @ get-text` / `tmux capture-pane`）。
缺对应程序时**显式 SKIP**，不假装通过。

为什么 tmux 要单列：TUI 的 scrollback 在 tmux 里由 **tmux 自己**持有
（`history-limit` + pane history），不在终端模拟器那层。`capture-pane -S -`
能直接读回「可见区 + 历史」，是这条语义最权威的观测面——`ClearType::Purge`、
alternate-screen 进出、嵌套清理都在这层出过问题。

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

`scripts/e2e-v2-real-terminal.sh`（kitty）、`scripts/e2e-v2-tmux.sh`（tmux）与
`scripts/e2e-v2-wezterm.sh`（WezTerm）已落地，逐维度判据都取自**对方的回读接口**，
不看 TUI 自述：

| 维度 | 判据 | 说明 |
|---|---|---|
| render | 屏幕里有 `❯`（composer）与 `ready`（状态行），且无 `panicked at` | Ctrl+L → Enter 打开会话后再读 |
| truecolor | 屏幕回读含 24-bit SGR | 三个环境都用 **T.416 冒号形式**（kitty/tmux `38:2:`、WezTerm `38:2::`），分号形式 `38;2;` 也认 |
| resize | 真实几何变化触发 SIGWINCH 后网格变化 **且屏幕内容确实重排** | kitty 改字号；tmux `resize-window`；WezTerm `split-pane`；见下方说明 |
| scrollback | 横幅**滚出可见区**后仍能从历史读回，且可见区里没有 | kitty 用 READY/GO 握手；tmux 用 `capture-pane -S -`；WezTerm 用 `--start-line -1000` |
| exit | Ctrl+Q 后回显 `TUI_EXIT=0` | 退出码回显 = 终端状态已还原、shell 拿回控制权 |
| 鼠标/复制 | 未申请鼠标追踪 | tmux 用 `#{mouse_any_flag}`；PTY 矩阵扫私有模式序列；kitty/WezTerm **无此查询面** |

| 终端 | scrollback | inline viewport | resize | truecolor | 鼠标/复制 | 状态 |
|---|---|---|---|---|---|---|
| **Kitty 0.48.2** | ✅ 已测 | ✅ 已测（render 维度） | ✅ 已测（SIGWINCH 重排） | ✅ 已测（`38:2:`） | ✅ 未开捕获 | **PASS**（2026-09-23） |
| Windows Terminal / PowerShell | 待测 | 待测 | 待测 | 待测 | 待测 | PENDING（无 Windows 实机） |
| **WezTerm 20260716** | ✅ 已测（`--start-line -1000`） | ✅ 已测（render 维度） | ✅ 已测（`split-pane` → SIGWINCH） | ✅ 已测（`38:2::`） | ⚠️ 无查询面 | **PASS**（2026-09-23） |
| Alacritty 0.17.0 | ⚠️ 无回读接口 | ⚠️ 无回读接口 | ⚠️ 无回读接口 | ⚠️ 无回读接口 | ⚠️ 无查询面 | **部分**（进程级，2026-09-23；见 §3.4） |
| iTerm2 | 待测 | 待测 | 待测 | 待测 | 待测 | PENDING（macOS 专属） |
| GNOME Terminal | 待测 | 待测 | 待测 | 待测 | 待测 | PENDING（环境未安装） |
| Konsole | 待测 | 待测 | 待测 | 待测 | 待测 | PENDING（环境未安装） |
| **tmux 3.7c** | ✅ 已测（`capture-pane -S -`） | ✅ 已测（render 维度） | ✅ 已测（`resize-window` → SIGWINCH） | ✅ 已测（透传 `38:2:`） | ✅ `mouse_any_flag=0` | **PASS**（2026-09-23） |
| SSH | 待测 | 待测 | 待测 | 待测 | 待测 | PENDING（无 sshd） |

**鼠标/复制**这一列对 12 个环境能力 profile 也已验证：全部 `mouse=off`。

### 3.0 tmux 专项：scrollback 语义

tmux 这层最该验证的是「TUI 写出去的行有没有真的进 tmux 的 history」。判据：

- 横幅先打、再灌 40 行 filler ⇒ 横幅必然滚出可见区；
- `tmux capture-pane -p -S -`（可见区 **+ 历史**）里必须有横幅；
- 同时 `tmux capture-pane -p`（**仅可见区**）里必须**没有**横幅。

两条都成立才说明「进的是 tmux history，不是还挂在屏幕上」。

**证伪**：把 history 读法换成只读可见区（去掉 `-S -`）→ 该条立刻变红
（实测 `history=False visible=False`）、`RESULT: FAIL`（已还原）。

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

### 3.3 tmux 实测输出（2026-09-23）

```text
[✓] scrollback：横幅已滚出可见区但仍在 history 里
[✓] 启动渲染：composer 提示符可见
[✓] 启动渲染：无 panic
[✓] 启动渲染：tmux pane 尺寸符合预期
[✓] truecolor：tmux 回读含 24-bit SGR
[✓] 鼠标/复制：TUI 未向 tmux 申请鼠标追踪
[✓] resize：pane 尺寸已变（真实 SIGWINCH 路径）
[✓] resize：重排后 UI 仍在（composer 可见）
[✓] resize：重排后无 panic
[✓] 退出：Ctrl+Q 后回显 TUI_EXIT=0
RESULT: PASS
```

**鼠标判据的证伪**：在 wrapper 里先 `printf "\033[?1003h"`（模拟应用申请鼠标）
→ `mouse_any_flag` 立刻变 `1`、该条变红、`RESULT: FAIL`（已实测后还原）。
说明这条不是恒真断言。

### 3.4 WezTerm 专项与 Alacritty 的能力边界

**WezTerm**（`scripts/e2e-v2-wezterm.sh`，9/9 PASS）——踩到两个坑，都写进脚本注释：

1. **必须直连 GUI 自己的 socket**。`wezterm cli --class X ...` 在本版本里**不会**
   去找 GUI 实例，而是连默认路径 `/run/user/$UID/wezterm/sock`；那儿没有 server
   时它会**自作主张 spawn 一个 `wezterm-mux-server`**，于是你读到的是一个全新
   默认 shell 的 pane —— 假绿/假红都可能是它。正确做法：取
   `gui-sock-<gui-pid>`，用 `WEZTERM_UNIX_SOCKET=<该 socket>` 跑 cli。
2. **无鼠标查询面**：cli 不暴露「应用有没有申请鼠标」，该列标 ⚠️ 而不是 ✅。

resize 走 `split-pane`（把 TUI 所在 pane 挤窄 → 真 SIGWINCH）——WezTerm cli
没有「改窗口尺寸」的命令，分屏是它唯一能程序化改 pane 几何的路径。

**Alacritty**（`scripts/e2e-v2-alacritty.sh`）——**只能做到进程级**，理由必须写清：

Alacritty **没有 IPC**（`alacritty msg` 只有窗口/配置子命令，既无屏幕回读也无按键
注入）。所以 kitty/tmux/WezTerm 那套「读回模拟器解出来的屏幕」在这里做不到。
本脚本只断言**进程级 + 环境级**事实，11 项：

- TUI 真的跑在 Alacritty 的 pty 里：`TERM=alacritty`、真 `/dev/pts/*`、
  行列数 = 请求的 100x35、**`infocmp alacritty` 可解析**（terminfo 缺失是
  crossterm 画不出来的经典原因）；
- TUI 持续存活、无 panic、写出了诊断日志；
- **`qaqh-tui doctor` 在 Alacritty 的 pty 内走完 discovery → pid 判活 →
  `/health` → open 握手并返回 `[4] OK`**（拿 client session + lease）。
  这条比在日志里 grep "starting new connection" 硬——后者只证明「发起过尝试」。

**渲染 / scrollback / resize / Ctrl+Q 退出不在能力内**，需人工视觉确认；
脚本末尾会打印 `RESULT: PASS(partial)` 而不是 `PASS`，避免被当成完整覆盖。
证伪：把 doctor 的 `QAQH_DATA_DIR` 指向不存在的目录 → 三条连通性断言全红、
`RESULT: FAIL`（已实测后还原）。

## 4. 已知风险

- Windows ConHost 的 `crossterm::ClearType::Purge` 当前只清可见 screen buffer，
  会话切换后历史清理能力需要 Windows 实机确认；
- `TERM=dumb` 的自动 PTY 已通过初始化/退出；真实滚动/复制行为仍未在 dumb
  终端上验证（dumb 终端本身无 scrollback 概念，价值有限）；
- tmux/SSH 的 cursor query、resize 和 scrollback 行为需要单独记录
  → **tmux 已补**（`scripts/e2e-v2-tmux.sh`，scrollback/resize/truecolor/exit
  全绿）；SSH 仍无 sshd，保持 PENDING。**不要**拿 `TERM=tmux-*` 环境变量分支
  当 tmux 真机证据——那是环境变量分支，不是真 multiplexer；
- 主题降级已通过「无崩溃」验证，颜色可读性仍需要人工视觉检查；
- **本机 compositor 下 `resize-os-window` 是空操作**（返回 0 但尺寸不变），
  所以 §3 的 resize 走改字号路径；真实窗口拖拽缩放仍需在有窗口管理器的
  实机上补一次；
- kitty / tmux / WezTerm 是**已开测**的真实终端环境（各自脚本全绿），Alacritty
  **只有进程级**（无 IPC，渲染需人工），其余保持 PENDING——环境能力分支
  （`TERM=tmux-256color` / `TERM=alacritty` 等）**不等于**该终端已测；
- **GPU 加速终端（WezTerm / Alacritty）的视觉正确性仍缺自动化手段**：WezTerm
  有 cli 回读所以能测，Alacritty 既无 IPC 也无回读，只能人工看。若将来要补，
  方向是 xdg-desktop-portal 截图 API（本机 compositor 下未验证可行性）。
