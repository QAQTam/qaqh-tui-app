# QAQH TUI v2 M6.2 / M6.3 增量 handoff（2026-09-21）

## 交接摘要

M6.2 已完成：历史分页、re-baseline 与会话切换的 scrollback 顺序已经落到
`V2TranscriptRuntime` 与 Agent View 终端生命周期，并通过单测和真实 PTY 冒烟。

M6.3 已开始，当前已覆盖：

- Workspace `Ctrl+L → Esc`；
- 真实 PTY resize；
- A/B 会话切换与 scrollback purge；
- 退出 alternate screen 重建 inline viewport 时的 cursor-query 竞态。
- daemon kill/restart 后 v2 自动重连。
- 80 次高频 resize 压力。
- v2 手动 `Ctrl+R` 重连。
- 终端环境能力矩阵（16/256/truecolor/Day/Terminal/NO_COLOR/dumb/tmux/screen/kitty/alacritty/SSH）。
- V2 commit runtime 性能基准与 timeline version 门。
- permission / ask fake provider + 真实 daemon PTY。
- `$PAGER` 挂起/恢复真实 PTY。
- `--v1` 显式回退（CLI 覆盖 `QAQH_V2_AGENT`）。

尚未覆盖：permission / ask / plan 的真实 daemon 故障注入、完整终端兼容矩阵。

## 当前工作树

本轮改动尚未提交。关键文件：

```text
src/ui/v2/runtime.rs
src/terminal/agent.rs
src/terminal/commit.rs
src/terminal/transcript.rs
scripts/e2e-v2-session-switch.sh
scripts/e2e-v2-reconnect.sh
scripts/e2e-v2-resize-stress.sh
scripts/e2e-v2-terminal-matrix.sh
scripts/e2e-v2-interactions.sh
docs/report/2026-09-21-v2-history-order-m6-2-report.md
docs/report/2026-09-21-v2-m6-3-pty-hardening-report.md
docs/report/2026-09-21-v2-m6-4-performance-baseline-report.md
docs/spec/2026-09-21-v2-terminal-compatibility-matrix.md
docs/spec/2026-09-20-v2终端提交协议-spec.md
docs/spec/2026-09-20-v2-v1-parity-matrix.md
docs/plan/2026-09-20-v2视觉与交互重构-plan.md
```

## 已验证

```text
cargo check --all-targets                         通过
cargo fmt --check                                 通过
cargo clippy --all-targets -- -D warnings         通过
cargo test --all-targets                          322 passed / 0 failed / 6 ignored
scripts/e2e-v2-session-switch.sh                  PASS
scripts/e2e-v2-reconnect.sh                       PASS
scripts/e2e-v2-resize-stress.sh                   PASS
RECONNECT_MODE=manual scripts/e2e-v2-reconnect.sh PASS
scripts/e2e-v2-terminal-matrix.sh                  PASS
MODE=permission scripts/e2e-v2-interactions.sh     PASS
MODE=ask scripts/e2e-v2-interactions.sh            PASS
MODE=pager scripts/e2e-v2-interactions.sh          PASS
```

PTY 冒烟输出：

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

高频 resize：

```text
tui exit=0 bytes=30963 resizes=80 cursor_queries=83
  [✓] 80 次 resize 已注入
  [✓] 触发多次 viewport 重建
  [✓] 无 cursor-position timeout
  [✓] 无 panic
RESULT: PASS
```

手动重连：

```text
mode=manual
tui exit=0 cursor_queries=3
  [✓] 无 cursor-position timeout
  [✓] 观察到 ready
  [✓] 观察到 lost
  [✓] 最终恢复到 ready
  [✓] 无 panic
  [✓] 手动重连动作可见
RESULT: PASS
```

终端能力矩阵：

```text
[✓] night-16 / night-256 / night-truecolor
[✓] day-256 / terminal-16 / no-color / term-dumb
RESULT: PASS
```

permission / ask：

```text
permission modal detected; approved with a       RESULT: PASS
ask modal detected; answered with 1              RESULT: PASS
pager body visible; pager returned               RESULT: PASS
```

M6.4 基准：

```text
cargo test render::bench::bench_v2_commit_runtime -- --ignored --nocapture
replay: 2533 blocks / 85.72 ms
高水位增量: 0 blocks / 3 µs
live delta ×20: avg 28 µs/帧
render: 24432 lines / 529.40 ms
```

规模曲线：

```text
cargo test app::render::bench::bench_v2_runtime_scale_curve -- --exact --ignored --nocapture
440 turns / 10123 blocks / replay 218.08 ms / delta 23 µs / render 1049.63 ms
```

曲线同时发现并修复了长会话完整 replay 被 4096 增量队列 cap 截断的问题；
完整 replay 现走 `enqueue_replay`，增量路径仍保持有界。

稳态内存对照：440 回合 v2 runtime steady 约 545KB，v1 淘汰后 render cache
约 2.3MB；R-03 长会话驻留目标已达标。

首帧对照：110 回合场景 v1 cache 约 31.4ms，v2 lazy replay+first-chunk 约
6.6ms；分块 replay 把 v2 从约 635ms 降到 6.6ms，R-01 已达标。

重连 PTY：

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

## 关键实现

### M6.2：scrollback 高水位

`ScrollbackFrontier` 以 `seen_turn_ids + max_turn_index` 过滤：

- 分页向前加载的旧回合只进入内存 timeline，不 emit 到 scrollback；
- re-baseline 只 emit 高于已提交高水位的新增 turn；
- 没有重叠且无法证明顺序时宁可不输出。

### M6.3：输入泵与终端重建

crossterm `EventStream` 会在后台 poll 线程中持有内部 event reader 锁。Agent View
离开 alternate screen、resize 或 purge 后重建 inline viewport 时，会执行
`cursor::position()`；两者并发会等待 2s 后超时退出。

当前输入泵改为专用线程 + 10ms 短轮询：

- 暂停时置停止位并 `join`；
- 确保 reader 锁释放后再执行终端重建；
- 所有 `enter_alternate` / `leave_alternate` / inline rebuild / pager / scrollback
  purge 路径都已接入暂停/恢复。

## 下一步

1. 评估 plan review 的 daemon 可控注入入口；当前 lap 内挂起路径恒为空。
2. 增加 `$PAGER` PTY 脚本。
3. 扩展真实终端模拟器矩阵；环境能力矩阵已自动化，Windows ConHost 的
   `ClearType::Purge` 仍需实机验证。
4. 扩展 M6.4 曲线与首帧/长会话对照，再进入 feature flag 与 M7。
