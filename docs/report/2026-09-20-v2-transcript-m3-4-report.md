# QAQH TUI v2 Transcript Runtime 报告（V2-M3.4）

> 状态：**M3 已完成；commit pump/runtime 已由 M4 Agent mode 消费**
> 日期：2026-09-20
> 上游报告：[`2026-09-20-v2-transcript-m3-3-report.md`](2026-09-20-v2-transcript-m3-3-report.md)
> 关联规范：
> [`2026-09-20-v2终端提交协议-spec.md`](../spec/2026-09-20-v2终端提交协议-spec.md) ·
> [`2026-09-20-v2-v1-parity-matrix.md`](../spec/2026-09-20-v2-v1-parity-matrix.md)

---

## 0. 结论

M3.4 完成 transcript projector 与 terminal scrollback 之间的提交运行时：

- 新增 `TranscriptCommitPump`；
- 新增 `V2TranscriptRuntime`；
- 队列有界，避免异常事件率造成内存无界增长；
- 队列条目必须为 `Sealed`；
- `drain_emittable` 通过 `TranscriptCommitLedger` 做最终幂等裁决；
- `sync_turn` / `replay_all` 返回已经过 ledger 的 `PendingCommit`；
- V1 默认路径不受影响。

M3 的最终收口由后续 M4.1/M4 完成：`--v2-agent` 已消费本层
`V2TranscriptRuntime`，真实 timeline 的 sealed block 会经 ledger 幂等提交到
scrollback，live block 继续留在 inline viewport。

真实 `--v2-agent` 模式没有在 M3.4 强行打开：当前 inline Agent 外壳尚未由 M4 完成，
提前把该队列接到 V1 全屏路径会污染默认 scrollback。M4 增加 V2 viewport 后直接消费
本层接口即可。

---

## 1. Commit Pump

```text
TranscriptCommitPump
  pending: VecDeque<PendingCommit>
  cap: usize (默认 4096)
  dropped: u64
  ledger: TranscriptCommitLedger
```

规则：

- `enqueue` 过滤非 `Sealed` block；
- 超过 cap 时淘汰最旧 pending，并累计 `dropped`，避免无界内存；
- `drain_emittable` 逐项交给 ledger：
  - `Emit` → 返回给 terminal 渲染；
  - `Duplicate` / `Conflict` → 丢弃，不重复写 scrollback；
- 返回的 block 已被 ledger 标记为 `Committed`。

---

## 2. Runtime

```text
V2TranscriptRuntime
  projector: V2TranscriptState
  pump: TranscriptCommitPump
```

接口：

- `sync_turn(seed, turn)`：增量事件 → 首次 Sealed block → pending commit；
- `replay_all(seed, turns)`：rebaseline / resize / session switch 全量重放；
- `reset_from_turns(turns)`：重建 projector 的已见状态；
- `clear()`：清空 projector 状态。

职责边界：

- `TimelineModel` 负责 wire 事件归约；
- `V2TranscriptState` 负责判断何时从 Live 进入 commit plan；
- `TranscriptCommitPump` 负责队列、背压与 ledger 裁决；
- terminal/main loop 只负责把 `PendingCommit` 渲染为 `insert_before`。

---

## 3. 安全与性能

- pending 队列硬上限 4096；
- 淘汰计数保留，后续可接入可观测提示；
- 只允许 Sealed block 进入队列；
- replay 重复数据由 ledger 去重，不依赖调用方判断；
- `CommitLedger` / pump 可 Clone，便于会话状态复制与测试；
- 不新增 wire 依赖，不改后端契约。

---

## 4. 测试

新增/更新：

```text
terminal::transcript::tests::*                 6
ui::v2::runtime::tests::*                      2
```

重点用例：

- `pump_emits_in_order_and_deduplicates_replay`
- `pump_ignores_live_blocks_and_bounds_queue`
- `runtime_emits_user_then_sealed_answer_once`
- `runtime_replay_uses_ledger_deduplication`

---

## 5. 验证结果

```text
cargo fmt --check                            # 通过
cargo clippy --all-targets -- -D warnings    # 通过
cargo test --all-targets                     # 284 passed / 0 failed / 6 ignored
scripts/tests/ci-linux-parse-test.sh         # 15 passed / 0 failed
```

---

## 6. 下一步

M4：V2 Agent viewport 外壳。

- 增加 `--v2-agent` / feature flag；
- inline viewport 渲染 live transcript；
- main loop 消费 `PendingCommit` 并调用 `insert_before`；
- V1 默认路径继续走原全屏 renderer；
- 完成后才把 M3 runtime 接到真实事件循环。
