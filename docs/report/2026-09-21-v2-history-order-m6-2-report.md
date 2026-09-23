# QAQH TUI v2 M6.2 历史分页与 Scrollback 顺序报告

## 0. 结论

V2-M6.2 已完成。Agent View 现在区分“timeline 内存窗口”和“终端 scrollback
已提交序列”：

- `load_older` / re-baseline 带入的旧回合不会再倒灌到 scrollback 尾部；
- re-baseline 只提交高于已提交高水位的新增回合；
- 会话切换先清屏与清 scrollback，再按新 seed 的权威快照完整重放；
- 清屏重放会重置该 seed 的 commit ledger，避免重放被误判为重复；
- 其他 seed 的 ledger 不受影响。

## 1. 问题

`TimelineModel::prepend_older` 会把更早回合插到内存窗口头部，这是 v1 全屏
自绘滚动所需的行为。但 V2 Agent View 使用终端 scrollback：

```text
已有 scrollback: [turn 10] [turn 11] [turn 12]
分页加载后内存:  [turn 8] [turn 9] [turn 10] [turn 11] [turn 12]
```

`insert_before` 只能把内容追加到当前 viewport 上方，不能插入到已经写入的
`turn 10` 之前。若继续让 projector 处理新增的 `turn 8/9`，终端实际顺序会变成：

```text
[turn 10] [turn 11] [turn 12] [turn 8] [turn 9]   # 错误
```

re-baseline 若返回一个向前扩展的窗口，也会触发同样的问题。会话切换则相反：
如果不清 scrollback，新会话历史只能追加到旧会话后面，无法形成独立的会话视图。

## 2. 实现

### 2.1 ScrollbackFrontier

`src/ui/v2/runtime.rs` 新增 `ScrollbackFrontier`：

- `seen_turn_ids`：已进入 projector 的稳定 turn 身份；
- `max_turn_index`：已提交内容的全局高水位；
- `bootstrapped`：首次同步前允许全量投影。

每次同步只从安全边界开始：

1. 已见过的 turn 必须重新同步，用于接住同一活动 turn 后续到达的 `BlockSealed`
   / `TurnSealed`；
2. `turn_index > max_turn_index` 的 turn 可以视为新增；
3. 从未见过且 `turn_index <= max_turn_index` 的 turn 是旧页或旧基线，跳过；
4. 没有已知重叠、也没有可证明的新高水位时返回空计划，优先不重复输出。

### 2.2 增量同步与会话重放分离

`V2TranscriptRuntime` 现在提供两条明确路径：

- `sync_timeline`：处理实时事件、分页、re-baseline，只输出高水位之后的新内容；
- `replay_from_scratch`：只在调用方已经清空 scrollback 后使用，先重置该 seed
  的 ledger，再按权威快照完整重放。

`TranscriptCommitPump::reset_seed` 与 `CommitLedger::forget_seed` 保证重置是
seed 隔离的；切换会话不会破坏其他会话的去重记录。

### 2.3 会话切换终端协议

`TerminalHost::purge_scrollback_for_replay` 执行：

```text
Clear(All) + Clear(Purge) + MoveTo(0, 0)
重建 Viewport::Inline
```

`AgentState::sync` 检测 active seed 变化并返回 `reset_scrollback = true`。
`commit_pending` 先 purge，再写入新 seed 的完整已封口 transcript。

## 3. 测试

新增 7 个回归测试：

```text
ui::v2::runtime::tests::prepending_older_turns_does_not_emit_them_into_scrollback
ui::v2::runtime::tests::rebaseline_emits_only_turns_after_the_high_water_mark
ui::v2::runtime::tests::replay_from_scratch_resets_only_the_seed_ledger
terminal::transcript::tests::reset_seed_allows_replay_after_scrollback_purge
terminal::transcript::tests::reset_seed_keeps_other_seed_ledger
terminal::commit::tests::forgetting_a_seed_keeps_other_seed_records
terminal::agent::tests::session_switch_resets_scrollback_and_replays_each_seed
```

关键证伪方式：

- 去掉 `sync_start` 的高水位过滤，`prepending_older_turns_does_not_emit...`
  会看到旧 turn 被 emit；
- 让 `replay_from_scratch` 不重置 ledger，会话切回测试的 pending 数量会变为 0；
- 让 `forget_seed` 清空全表，其他 seed 的 replay 去重测试会失败。

## 4. 验证结果

```text
cargo check --all-targets                         通过
cargo fmt --check                                 通过
cargo clippy --all-targets -- -D warnings         通过
cargo test --all-targets                          322 passed / 0 failed / 6 ignored
```

测试数由 M6.1 的 315 增至 322。

真实 daemon + PTY 冒烟：

```text
scripts/e2e-v2-session-switch.sh
tui exit=0 cursor_queries=7
  [✓] 无 cursor-position timeout
  [✓] 两个会话标题均渲染
  [✓] 至少两次 scrollback purge
  [✓] alternate screen 进出
  [✓] 无 panic
  purge_count=2
RESULT: PASS
```

该脚本同时覆盖 Workspace `Ctrl+L → Esc`、真实 resize 和 A/B 会话切换。

该 PTY 冒烟同时暴露并修复了一个 M6.3 级终端竞态：离开 alternate screen
重建 inline viewport 时，后台 `EventStream` 会持有 crossterm 内部 event reader
锁，导致 `cursor::position()` 等待 2s 后超时并让 Agent View 退出。现在所有可能
重建终端对象的路径都会先暂停输入泵，完成终端操作后再恢复。输入泵改为
10ms 短轮询线程；暂停时置停止位并 `join`，从机制上保证 reader 锁已释放，
不依赖固定 sleep 猜测释放时间。

## 5. 已知边界

- `crossterm::ClearType::Purge` 在 Unix 映射为 `ESC[3J`；Windows ConHost 后端
  当前只清可见 screen buffer，历史清理能力仍需在 M6.3 的终端矩阵中实测。
- 本轮只锁定内存状态机与 commit 计划，尚未用终端模拟器逐字节断言
  “会话切换前后的 scrollback 文本”。
- Agent View 的 `load_older` 仍可更新内存窗口，但不会进入 scrollback；完整历史
  浏览 UI 仍按 terminal spec §10 留给 Workspace 方向。
- re-baseline 若完全没有已知 turn 重叠，也没有可证明的新 `turn_index`，本轮选择
  不输出；后续若真实终端观察到“新内容延迟”，应先补 PTY 证据再放宽判据。

## 6. 下一步

进入 V2-M6.3：真实 daemon + PTY 硬化，覆盖 permission / ask / plan、
Workspace 进出、resize、断线重连、多会话切换，并把本轮 Windows scrollback
purge 边界纳入终端兼容矩阵。
