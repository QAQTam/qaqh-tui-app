# QAQH TUI v2 Transcript Commit 报告（V2-M3.2）

> 状态：**M3.2 已完成；M3.3 parity 与事件接线待续**
> 日期：2026-09-20
> 上游报告：[`2026-09-20-v2-transcript-m3-1-report.md`](2026-09-20-v2-transcript-m3-1-report.md)
> 关联规范：
> [`2026-09-20-v2终端提交协议-spec.md`](../spec/2026-09-20-v2终端提交协议-spec.md) ·
> [`2026-09-20-v2-v1-parity-matrix.md`](../spec/2026-09-20-v2-v1-parity-matrix.md)

---

## 0. 结论

M3.2 把 V2 block 接入了既有 commit ledger：

- `TranscriptBlock` 增加 `turn_id` 与稳定 `content_fingerprint`；
- 新增 `TranscriptCommitLedger`，统一映射 `CommitId + content_hash`；
- 只有 `Sealed` block 可提交；
- `Emit` 成功后才推进到 `Committed`；
- inline 原型不再提交裸文本行，而是提交 V2 主题化 block；
- replay 重置临时副本到 `Sealed`，由 ledger 返回 duplicate。

---

## 1. 提交映射

```text
TranscriptBlock.id          -> CommitId.block_id
TranscriptBlock.turn_id     -> CommitId.turn_id
TranscriptBlock.revision    -> CommitId.revision
content_fingerprint()       -> content_hash
```

`content_fingerprint()` 使用长度前缀字段，避免内容拼接歧义；包含：

- block id / turn id / revision / state；
- User / Assistant / Thinking 正文；
- Tool 的名称、状态、summary、output、diff、progress、failure、duration、bytes；
- System 正文与 level。

---

## 2. 状态与幂等

```text
Live
  -> seal()
Sealed
  -> TranscriptCommitLedger::commit_block()
       Emit      -> Committed
       Duplicate -> 保持 Sealed，跳过输出
       Conflict  -> 保持 Sealed，拒绝输出
```

关键不变式：

- live block 不进入 ledger；
- duplicate 不重复写 scrollback；
- conflict 不覆盖原记录；
- inline replay 使用同一 id / revision / 内容，只改变生命周期副本；
- 进程退出后 terminal scrollback 保留历史，ledger 不要求跨进程恢复。

---

## 3. Inline 接线

`--v2-inline` 现在：

1. Enter 生成 `TranscriptBlock::User`；
2. seal 后走 `TranscriptCommitLedger`；
3. `Emit` 时按当前终端宽度调用 V2 `render_block()`；
4. 使用 `Terminal::insert_before()` 提交主题化 block；
5. `r` 重放时恢复临时 block 到 Sealed，验证 duplicate；
6. 用户输入仍只存在 inline live viewport，不提前写 scrollback。

这完成了 M1 提交协议与 M3 主题渲染的第一次真实组合。

---

## 4. 测试

新增/更新：

```text
terminal::transcript::tests::*                4
terminal::inline::tests::*                    5（更新提交块路径）
ui::v2::adapter::tests::*                     3
ui::v2::transcript::tests::*                 11
```

重点用例：

- `emits_once_and_marks_block_committed`
- `replay_is_duplicate_and_does_not_emit_again`
- `same_identity_with_changed_content_is_conflict`
- `live_block_is_not_committable`
- `committed_block_contains_prompt_and_text`
- `committed_block_uses_theme_tokens`
- `replay_uses_same_identity_and_is_duplicate`

---

## 5. 验证结果

```text
cargo fmt --check                            # 通过
cargo clippy --all-targets -- -D warnings    # 通过
cargo test --all-targets                     # 274 passed / 0 failed / 6 ignored
scripts/tests/ci-linux-parse-test.sh         # 15 passed / 0 failed
```

---

## 6. 下一步

V2-M3.3：事件接线与 parity。

- 把 `BlockCheckpoint` / `BlockSealed` / `TurnSealed` 接入 V2 commit 时机；
- 用 adapter 接真实 timeline / re-baseline / reconnect；
- 完成 resize 重放与冲突诊断；
- 对照 parity matrix 补齐 User / Assistant / Thinking / Tool 行为；
- 之后再进入 V2-M4 Composer / Status / Shortcuts。
