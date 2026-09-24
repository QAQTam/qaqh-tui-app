# TUI Ringing v2 reducer 交接（2026-09-24）

> 状态：**第一版已完成，PR #49 open；等 daemon P0-3 与后端 #323 typed payload。**
> 分支：`feat/alpha1-v2-bootstrap`
> 锚点：`tui-ringing-v2-types-2026-09-24` @ `a43a8bc`

## 0. 一句话

TUI 已经把 Ringing v2 的 cursor、reset、pending interaction、driver epoch
核心状态机做成纯 reducer，并通过 `qaqh-client` 的 typed bootstrap/event/reset
适配接上最小锚点。daemon `/ringing/v2` 与 interaction/driver payload 内部
匹配仍待后端 P0-3 / #323。

## 1. 本切片交付

### 1.1 M7 UI 默认

- `qaqh-tui` alpha1 默认进入 V2 Agent View。
- `--v1` 仍强制回退 v1 全屏。
- `--v2-agent` / `QAQH_V2_AGENT` 保留为显式兼容入口。
- **协议仍是 Ringing v1**；协议切换单独等 daemon P0-3。

### 1.2 纯 reducer

文件：`src/app/ringing_v2.rs`

已锁不变量：

1. reliable 只按 `(fact_seq, projection_index)` 严格前进；
2. exact repeat 返回 `Duplicate`，旧位置返回 `Stale`；
3. `server_epoch` / `log_id` 不匹配拒绝；
4. replaceable 只更新 revision，不推进 cursor；
5. ephemeral 不改变持久状态；
6. reset 只标记 pending，保留旧模型；
7. 新 bootstrap 验证通过后原子替换模型；
8. interaction 按 `interaction_id` 幂等，resolved/expired 后不得重开；
9. driver epoch 单调；旧 epoch `Stale`，同 epoch 不同 holder `Conflict`。

### 1.3 typed adapter

`src/app/ringing_v2.rs` 当前提供：

- `bootstrap_from_client(&ClientV2Bootstrap)`
- `event_meta_from_client(&ClientV2Event)`
- `payload_family(&ClientV2Event)`
- `reset_from_client(&ClientV2Reset)`

适配只通过 `qaqh-client`，不直接依赖 `qaqh-ringing` / `qaqh-domain` /
`qaqh-session`。

## 2. 当前未接线

`RingingV2SessionModel` 目前是纯状态机，尚未挂入 `SessionState` / `App` 的
v2 协议循环。原因：

- daemon `/ringing/v2` 尚未实现（P0-3）；
- `ClientV2Payload` 的 `ControlDelta` 内部类型不可从 `qaqh-client` 命名；
- `DriverChanged` 尚未进入 canonical projection；
- pending interaction 的 request 载荷/replay 保证待明确。

后三项见后端 issue：`QAQ-Harness/qaqh-backend#323`。

## 3. 验证

```text
cargo test --all-targets -- --test-threads=1      348 passed / 9 ignored
cargo clippy --all-targets -- -D warnings         PASS
scripts/static-gates.sh                            PASS
scripts/perf-gate.sh                               PASS
scripts/smoke-tui.sh                               PASS
TUI_ARGS=--v1 scripts/smoke-tui.sh                 PASS
e2e-v2-faults.sh 六模式                            PASS
e2e-v2-interactions.sh 九模式                      PASS
```

锚点 daemon：

```text
rev: a43a8bcfb01e4cf0f97154f942d874f60cd54aa6
sha256: 40de1873b92b0403682fa60b10ad6e934f9be23fece7e802087e8aff971d9f9b
```

## 4. 下一步

1. 等后端 P0-3：daemon v2 open/bootstrap/subscribe/replay/reset 最小闭环。
2. 等后端 #323：typed ControlDelta / DriverChanged / pending request。
3. 把 reducer 挂入 `SessionState`，实现 v2 bootstrap -> subscribe -> replay ->
   live -> reset 的完整生命周期。
4. 用后端 v2 fixture 覆盖 `V2-C1..C7` / `V2-R1..R4` / `V2-D1..D3`。
5. 协议切换完成后再删 `#![allow(dead_code)]` 并切换默认协议到 v2。
