# Ringing v2 fixture matrix 报告（2026-09-25）

> 状态：TUI 侧 Linux fixture 矩阵已落地；`V2-W1` 仍待 Windows 实机执行。
> 关联：`QAQ-Harness/qaqh-tui-app#46` task 5；PR #52。

## 1. 结论

本切片把 spec §13 中 TUI 可执行的矩阵行全部落成回归测试，并移除了
`src/app/ringing_v2.rs` 顶部的临时 `#![allow(dead_code)]`。

同时修复了一个 `V2-T2` 实质缺口：

- reset pending 时，bootstrap 必须匹配 reset 宣告的 `server_epoch` / `log_id`，
  且 snapshot baseline 不得早于 reset 给出的 `snapshot_cursor`；
- 同 epoch 内较旧的 bootstrap `state_revision` 不得回滚新 SessionModel；
- reducer 拒绝 bootstrap 后，App 立即终止，不再继续投影旧 UI / 覆盖当前
  `client_session_id`；
- 旧 epoch 的 reliable 帧继续由统一 reducer 门控丢弃。

## 2. 矩阵映射

| 行 | TUI 回归 |
|---|---|
| `V2-C1` | `app::ringing_v2::tests::v2_c1_snapshot_then_subscribe_is_gap_free_and_duplicate_free` |
| `V2-C2` | `app::ringing_v2::tests::v2_c2_reliable_reconnect_advances_in_global_lexicographic_order` |
| `V2-C3` | `app::ringing_v2::tests::v2_c3_replaceable_reconnect_is_latest_current_without_cursor_advance` |
| `V2-C4` | `app::ringing_v2::tests::v2_c4_ephemeral_never_persists_or_replays` |
| `V2-C5` | `app::ringing_v2::tests::v2_c5_epoch_log_mismatch_is_rejected_and_log_reset_maps_typed_reason` |
| `V2-C6` | `app::ringing_v2::tests::v2_c6_cursor_expired_keeps_old_state_until_rebaseline` |
| `V2-C7` | `app::ringing_v2::tests::v2_c7_snapshot_missing_is_read_only_and_does_not_guess_history` |
| `V2-R1..R3` | `app::ringing_v2::tests::v2_r1_r3_reconnect_restores_permission_ask_and_plan_by_stable_id` |
| `V2-R4` | `app::ringing_v2::tests::v2_r4_first_answer_wins_and_terminal_interaction_cannot_reopen` |
| `V2-D1` | `app::ringing_v2::tests::v2_d1_vacant_seat_claim_is_applied_with_monotonic_epoch` |
| `V2-D2` | `app::ringing_v2::tests::v2_d2_busy_driver_rejects_other_claimants_stably` |
| `V2-D3` | `app::ringing_v2::tests::v2_d3_handover_rejects_old_epoch_and_same_epoch_conflicts` |
| `V2-T1` | `scripts/static-gates.sh` G1–G4 |
| `V2-T2` | reducer：`v2_t2_stale_bootstrap_cannot_rollback_newer_snapshot` / `v2_t2_old_bootstrap_after_reset_must_match_reset_epoch_and_log` / `v2_t2_old_bootstrap_before_reset_baseline_is_rejected`；App：`v2_t2_old_bootstrap_response_and_frame_cannot_rollback_after_reset` |
| `V2-V1` | 不在 TUI 实现；由 daemon 负责 v1 cursor 映射，本仓不读存储布局 |
| `V2-W1` | 同一组 fixture 全部为平台无关 Rust 测试；待 Windows alpha 实机执行 |

## 3. 验证

```text
cargo test --all-targets --offline -- --test-threads=1     421 passed / 0 failed / 9 ignored
cargo clippy --all-targets --offline -- -D warnings        PASS
scripts/static-gates.sh                                     PASS
scripts/perf-gate.sh                                        PASS
cargo fmt --all --check                                     PASS
```

## 4. 遗留

- `V2-W1` 需要 Windows 环境运行同一测试过滤：
  `cargo test --bin qaqh-tui v2_ -- --test-threads=1`。
- 本切片不切换默认协议；v2 默认切换仍按 #46 后续步骤执行。
