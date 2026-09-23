# QAQH TUI v2 M6.3 PTY 硬化报告（增量）

> 日期：2026-09-21
> 状态：进行中

## 0. 结论

M6.3 已覆盖三类真实终端路径：

1. Workspace 进出与 resize；
2. A/B 会话切换与 scrollback purge；
3. daemon kill/restart 后的 v2 自动重连。

本轮同时修复了 PTY 才能稳定复现的 crossterm reader 锁竞态。permission / ask /
plan 的真实故障注入与完整终端兼容矩阵仍未完成，因此 M6.3 尚未收口。

## 1. 终端重建竞态

### 复现

v2 Agent View 在以下路径会重建 inline viewport：

- 离开 alternate screen；
- resize 导致动态高度变化；
- 会话切换清 scrollback 后重建；
- `$PAGER` 返回。

重建时 `Terminal::with_options(Viewport::Inline)` 会执行 `cursor::position()`。
此前输入由 crossterm `EventStream` 承担，其后台 poll 线程持有 crossterm 内部
event reader 锁；PTY 中 cursor query 会等锁 2s 后超时：

```text
Error: The cursor position could not be read within a normal duration
```

### 修复

输入泵改为专用线程 + 10ms 短轮询：

- 每次 poll 后主动释放 reader 锁；
- 暂停时置停止位并 `join`；
- 确保输入线程完全退出后再执行终端重建；
- 所有 `enter_alternate` / `leave_alternate` / inline rebuild / pager /
  scrollback purge 路径统一接入暂停与恢复。

相比固定 sleep，这个方案有确定性的锁释放边界，不依赖猜测延迟。

## 2. PTY 冒烟

### 2.1 Workspace / resize / 会话切换

脚本：

```text
scripts/e2e-v2-session-switch.sh
```

结果：

```text
tui exit=0 cursor_queries=7
  [✓] 无 cursor-position timeout
  [✓] 两个会话标题均渲染
  [✓] 至少两次 scrollback purge
  [✓] alternate screen 进出
  [✓] 无 panic
  purge_count=2
RESULT: PASS
```

覆盖动作：

```text
Ctrl+L → Esc              Workspace 进出
TIOCSWINSZ + SIGWINCH     真实 resize
Ctrl+L → Down → Enter      A/B 会话切换
```

### 2.2 断线重连

脚本：

```text
scripts/e2e-v2-reconnect.sh
```

结果：

```text
daemon#1 killed at t=7
daemon#2 started at t=30
tui exit=0 cursor_queries=3
  [✓] 无 cursor-position timeout
  [✓] 观察到 ready
  [✓] 观察到 lost
  [✓] 最终恢复到 ready
  [✓] 无 panic
RESULT: PASS
```

脚本使用隔离 data root，先打开一个真实会话，再 kill daemon#1。TUI 进入
`lost` 后，daemon#2 使用同一 data root 启动；客户端自动刷新 discovery 并恢复
`ready`，无需重启 TUI。

手动模式：

```text
RECONNECT_MODE=manual scripts/e2e-v2-reconnect.sh
mode=manual
daemon#1 killed at t=7
daemon#2 started at t=33
tui exit=0 cursor_queries=3
  [✓] 无 cursor-position timeout
  [✓] 观察到 ready
  [✓] 观察到 lost
  [✓] 最终恢复到 ready
  [✓] 无 panic
  [✓] 手动重连动作可见
RESULT: PASS
```

manual 模式在 daemon 仍 dead 时按一次 `Ctrl+R`，覆盖“正在重连 / 重连失败”
提示；daemon#2 启动后再按一次 `Ctrl+R`，最终恢复 `ready`。

### 2.3 高频 resize 压力

脚本：

```text
scripts/e2e-v2-resize-stress.sh
```

结果：

```text
tui exit=0 bytes=30963 resizes=80 cursor_queries=83
  [✓] 80 次 resize 已注入
  [✓] 触发多次 viewport 重建
  [✓] 无 cursor-position timeout
  [✓] 无 panic
RESULT: PASS
```

该脚本保持宽度 130 不变，连续切换 10 组终端高度，每次 `TIOCSWINSZ + SIGWINCH`
都触发动态 inline viewport 重算。83 次 cursor query 对应高频重建路径，输入泵
stop/join 未再出现 reader 锁竞争。

### 2.4 终端能力矩阵

脚本：

```text
scripts/e2e-v2-terminal-matrix.sh
```

结果：

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

正式记录见：

```text
docs/spec/2026-09-21-v2-terminal-compatibility-matrix.md
```

该矩阵覆盖环境能力组合，不替代 Windows Terminal / WezTerm / iTerm2 / tmux /
SSH 等真实终端模拟器验证。

### 2.5 permission / ask 真实 daemon PTY

脚本：

```text
MODE=permission scripts/e2e-v2-interactions.sh
MODE=ask scripts/e2e-v2-interactions.sh
```

实现方式：

- 本地 fake OpenAI-compatible provider；
- 通过真实 daemon HTTP 控制面预创建会话，避免 `Ctrl+N` 创建竞态；
- TUI 连接真实 daemon 并发送真实用户消息；
- permission：provider 返回 `exec` tool call，触发真实权限引擎；
- ask：provider 返回 `ask` tool call，触发真实 `InteractionRequested`。

permission 结果：

```text
permission modal detected; approved with a
  [✓] permission modal visible
  [✓] permission approved
  [✓] no cursor-position timeout
  [✓] no panic
RESULT: PASS
```

ask 结果：

```text
ask modal detected; answered with 1
  [✓] ask modal visible
  [✓] ask answered
  [✓] no cursor-position timeout
  [✓] no panic
RESULT: PASS
```

plan review 当前 lap 内挂起路径在后端代码中恒为空，未找到可控触发入口；不计作
覆盖证据。

### 2.6 `$PAGER` 挂起/恢复

脚本：

```text
MODE=pager scripts/e2e-v2-interactions.sh
```

fake provider 持续流式 reasoning，TUI 在活动回合中执行
`Ctrl+T → e`，`PAGER=cat` 挂起终端、输出全文并恢复 inline viewport。

结果：

```text
Ctrl+Q sent after pager
  [✓] pager body visible
  [✓] pager returned
  [✓] no cursor-position timeout
  [✓] no panic
RESULT: PASS
```

## 3. 门禁

```text
cargo check --all-targets                         通过
cargo fmt --check                                 通过
cargo clippy --all-targets -- -D warnings         通过
cargo test --all-targets                          322 passed / 0 failed / 6 ignored
scripts/tests/ci-linux-parse-test.sh              15 passed / 0 failed
```

## 4. 未覆盖 / 风险

- permission / ask 已通过 fake provider + 真实 daemon PTY；plan review 尚无
  可控触发入口；
  → **2026-09-23 更新**：三条否定路径（`permission-deny` / `ask-dismiss` /
  `plan-reject`）已落地并全绿，判据改用后端权威证据（timeline 快照 + provider
  上下文 + 会话事实账本哈希）；`plan-reject` 在锚点升到 v2.0.0 RC 后扩成
  **9 条子断言**。详见
  [`2026-09-23-v2-interaction-negative-paths-report.md`](2026-09-23-v2-interaction-negative-paths-report.md)
  与 [`2026-09-23-backend-anchor-v2.0.0-rc-report.md`](2026-09-23-backend-anchor-v2.0.0-rc-report.md)。
  后者记录了钩子保真度缺口由后端 #301 收口的前后对照。
- Windows ConHost 的 `ClearType::Purge` 是否清理全部历史仍需实机验证；
- tmux / SSH / WezTerm / iTerm2 / Alacritty / Kitty / GNOME Terminal / Konsole
  尚未完成真实模拟器矩阵；环境能力矩阵已自动化；
  → **2026-09-23 更新**：Kitty / tmux / WezTerm 已实测通过，Alacritty 仅进程级
  （无 IPC）；见 [`2026-09-21-v2-terminal-compatibility-matrix.md`](../spec/2026-09-21-v2-terminal-compatibility-matrix.md) §3；
- 输入泵暂停期间的事件不重放；
- `$PAGER` 路径已有真实 PTY 自动化；
- **四个故障钩子已全部接线并全绿**（2026-09-23 收口）：
  `scripts/e2e-v2-faults.sh` 六个模式 `none` / `lagged` / `gap` / `ack-delay` /
  `ack-hang` / `session-404` **全部 PASS**。转绿路径：
  - `none` / `gap` / `ack-*`：**后端两处修复**（#42 的 seq 空间被凭空消耗；
    seal 时无条件裁剪 journal 导致回合中途重基线后补不齐）——
    见 [`2026-09-23-backend-issue42-root-cause-and-fix-report.md`](2026-09-23-backend-issue42-root-cause-and-fix-report.md)；
  - `lagged`：TUI 侧**渲染截断**缺陷——`conn_error` span 写死截断 28 列，
    把 `服务端终止流（lagged…` 的原因切掉（U-07 的诊断看不见），改为宽度感知；
  - `session-404`：TUI 侧**断言文案**缺陷——钩子拦的是 bootstrap 请求，
    原断言却找 timeline 流 404 那条路径的「会话不存在（404）」文案，
    已对齐到实际渲染的 `bootstrap 失败[seed]: HTTP 404`。
- 真实终端矩阵已按 §3.5 收口：Kitty/tmux/WezTerm 实测通过，Alacritty 仅进程级，
  其余终端**决定不逐个支持**（唯一保留例外是 Windows ConHost 的 `ClearType::Purge`）。
- **后端 #45（仍 open）**：daemon 每回合在 `session-title` 线程 panic
  （`there is no reactor running`），**LLM 标题总结实际从未生效**（落盘标题停在
  截断版）。已提 issue 并附 backtrace 与建议修法；TUI 侧**未**为它加断言，避免把
  后端缺陷钉进门禁。见 <https://cnb.cool/QAQ-Harness/qaqh-tui-app/-/issues/45>。

## 5. 下一步

1. 优先确认后端是否提供 permission / ask / plan 的可控测试钩子；
   → **2026-09-23**：钩子已到（后端 issue #41 / `QAQH_TEST_*` spec），TUI 侧
   permission / ask / plan 肯定+否定路径均已接线；剩余四个故障钩子被 issue #42 阻塞；
2. 若无 hook，先建立真实 LLM/daemon 的手工交互记录，不把模拟当端到端证据；
3. 增加 `$PAGER` PTY 脚本；→ **已完成**；
4. 开始终端兼容矩阵记录。
