# TUI Ringing v2 production wiring handoff（2026-09-25）

> 状态：**reducer 已挂入生产 SessionState；bootstrap / event / reset / interaction / driver 接线完成；Linux fixture 矩阵已完成。**
> 基分支：`origin/main`（`162e128`），并包含 TUI 契约同步提交 `de78c65`。
> 关联：`QAQ-Harness/qaqh-tui-app#46` task 2–5。

## 1. 本切片交付

### 1.1 SessionState 持有 reducer

- `SessionState` 新增 `ringing_v2: RingingV2SessionModel`。
- bootstrap 成功后先应用 `bootstrap_from_client`，再恢复 UI 面板；新快照通过校验才原子替换 reducer。
- reset 到达时先 `begin_reset`，旧 UI 保留；新 bootstrap 到达后再替换。

### 1.2 v2 事件统一门控

`App::handle_v2_event` 在任何 payload 分派前先执行 reducer：

- reliable 严格按 `(fact_seq, projection_index)` 前进；
- duplicate / stale / epoch mismatch / log mismatch / reset-pending 事件不进入 UI 分派；
- replaceable 只更新 revision，ephemeral 不改变持久状态。

### 1.3 interaction 幂等

- `InteractionRequested` 先进入 reducer，只有 `Requested` 才创建 UI 面板；
- `InteractionResolved` / `InteractionExpired` 先更新 reducer 终态，再清理面板；
- bootstrap pending set 仍作为重连权威来源。

### 1.4 driver capability

- `DriverChanged` 进入 reducer，旧 epoch / 同 epoch 冲突 holder 被拒绝。
- App 保存当前 v2 `client_session_id`，以 reducer holder 判断 driver。
- 空 seat 且 `can_claim=true` 时自动发起 canonical claim；holder 仍只从 reliable `DriverChanged` 更新。
- 非 driver 的发送、中止、模式切换、压缩、撤销进入只读态；交互应答不受影响。

### 1.5 fixture 矩阵与旧响应防护

- `V2-C1..C7` / `V2-R1..R4` / `V2-D1..D3` / `V2-T2` 已落成命名回归测试；
  `V2-T1` 由 `scripts/static-gates.sh` G1–G4 覆盖；`V2-V1` 不在 TUI 实现。
- reset pending 时 bootstrap 必须匹配 reset 的 `server_epoch` / `log_id`，且
  snapshot baseline 不得早于 reset cursor；同 epoch 较旧的 `state_revision`
  不再覆盖新 SessionModel。
- reducer 拒绝 bootstrap 后 App 立即终止，不再投影旧 UI，也不覆盖当前
  `client_session_id`；旧 epoch reliable 帧继续在 UI 分派前丢弃。
- 详细映射见
  `docs/report/2026-09-25-ringing-v2-fixture-matrix-report.md`。

## 2. 验证

```text
cargo test --all-targets --offline -- --test-threads=1    421 passed / 0 failed / 9 ignored
cargo clippy --all-targets --offline -- -D warnings       PASS
scripts/static-gates.sh                                    PASS
scripts/perf-gate.sh                                       PASS
cargo fmt --all --check                                    PASS
```

新增回归锁：

- duplicate reliable event 在 UI 分派前丢弃；
- reset 后事件保持 blocked，直到新 bootstrap；
- stale bootstrap / 旧响应不能回滚 SessionModel 或 lease 身份；
- driver holder 更新只读态，同 epoch 冲突不夺权；
- 非 driver 发送不消费 composer 且提示可见。

## 3. 仍未完成

- `V2-W1`：同一组 fixture 已保持平台无关，但本机无 Windows 环境，仍需 Windows
  alpha 实机执行 `cargo test --bin qaqh-tui v2_ -- --test-threads=1`。
- 本切片不切换默认协议；v2 默认切换按 #46 后续步骤执行。
