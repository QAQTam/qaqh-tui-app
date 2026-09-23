# QAQH TUI v2 Transcript Projector 报告（V2-M3.3）

> 状态：**M3.3 projector 已完成；App/terminal feature-flag 接线待续**
> 日期：2026-09-20
> 上游报告：[`2026-09-20-v2-transcript-m3-2-report.md`](2026-09-20-v2-transcript-m3-2-report.md)
> 关联规范：
> [`2026-09-20-v2终端提交协议-spec.md`](../spec/2026-09-20-v2终端提交协议-spec.md) ·
> [`2026-09-20-v2-v1-parity-matrix.md`](../spec/2026-09-20-v2-v1-parity-matrix.md)

---

## 0. 结论

M3.3 完成 V2 transcript 的事件投影层：

- 新增 `src/ui/v2/projector.rs`；
- 输入是既有 `TimelineModel` 归约后的 turn；
- 只有 block 首次从 `Live` 进入 `Sealed` 才产生提交候选；
- `TurnSealed` 会把仍为 Open 的 block 按封口处理；
- 支持 rebaseline 重建“已见”状态；
- 支持 resize / session switch 的 `replay_all` 全量重放计划；
- 支持后端 turn_id 原地 reopen 的世代重置。

本层不直接写终端，也不修改 V1 App 路径；真正接线应由后续 V2 feature flag
统一控制，避免默认全屏路径误写 scrollback。

---

## 1. 投影状态

```text
V2TranscriptState
  seen:
    (turn_id, user_text, block_id) -> Live | Planned
  turn_had_rounds:
    turn_id -> bool
```

规则：

- `Live` block：登记，不提交；
- `Sealed` block：首次出现时登记为 `Planned` 并返回候选；
- 再次同步同一 block：不重复返回；
- `reset_from_turns`：用权威快照重建 seen，后续增量不重复计划旧块；
- `replay_all`：按 timeline 顺序返回全部 sealed block，由 commit ledger 去重；
- `turn.rounds` 从非空变空：识别为 turn reopen，清除该 turn 的旧世代记录。

---

## 2. 与 TimelineModel 的分工

```text
RuntimeMsg::Timeline
  -> TimelineModel::apply()
       -> 维护 turn / round / block / tool 状态
  -> V2TranscriptState::sync_turn()
       -> 只决定“哪些 sealed block 值得进入 commit plan”
```

投影层不重新解释 TextDelta、ToolProgress、BlockCheckpoint 等 wire 语义；
这些仍由既有 reducer 单源负责。这样避免 V1/V2 在事件归约上产生第二套真相。

---

## 3. 生命周期

```text
TurnOpened
  -> user block Sealed
  -> commit plan: user

BlockOpened / TextDelta / ToolProgress
  -> block Live
  -> no commit plan

BlockSealed
  -> block Sealed
  -> commit plan: block

TurnSealed
  -> 未收到 BlockSealed 的 Open block 也被视为 Sealed
  -> commit plan: 剩余 block

TimelineRebaseline
  -> reset_from_turns() 标记权威已见状态
  -> replay_all() 生成全量重放计划
  -> ledger 负责 Duplicate / Conflict 去重
```

---

## 4. 测试

新增测试覆盖：

```text
ui::v2::projector::tests::*                    6
```

重点用例：

- `live_block_is_not_planned_until_sealed`
- `sealed_block_is_planned_once`
- `turn_sealed_seals_open_blocks_and_is_replayable`
- `rebaseline_reset_does_not_replan_old_blocks`
- `reopen_clears_previous_generation`
- `large_turn_is_projected_linearly`（1000 blocks）

---

## 5. 验证结果

```text
cargo fmt --check                            # 通过
cargo clippy --all-targets -- -D warnings    # 通过
cargo test --all-targets                     # 280 passed / 0 failed / 6 ignored
scripts/tests/ci-linux-parse-test.sh         # 15 passed / 0 failed
```

---

## 6. 下一步

App / terminal feature-flag 接线：

- V2 Agent View 模式下，在 `RuntimeMsg::Timeline` 后调用 `sync_turn`；
- 在 `TimelineRebaseline` 后调用 `reset_from_turns` + `replay_all`；
- 由 `TranscriptCommitLedger` 决定是否 emit；
- main loop 统一消费 commit plan 并调用 `Terminal::insert_before`；
- 保持 V1 默认全屏路径不进入该队列。
