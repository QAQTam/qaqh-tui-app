# TUI 锚点升级到 Ringing v2 daemon/interaction 锚点验证报告（2026-09-24）

## 0. 一句话

TUI 的 CI pin 与本机 anchor 从 `tui-ringing-v2-types-2026-09-24@a43a8bc`
（只有类型面）升到 `tui-ringing-v2-interaction-causation-2026-09-24@b77c251`
（daemon `/ringing/v2` 最小闭环 + interaction 因果）；换锚点只撞到一处破坏面
（`ClientV2ControlState` 由别名变手写结构体，测试 fixture 少必填字段），已在本仓
修好，全量门禁 + 真实 daemon 的 smoke / 故障六模式 / interaction 九模式全绿。

## 1. 验证锚点

| 项 | 值 |
|---|---|
| 后端 rev | `b77c2519f06f66c084bcb29d234e4a9c008777c5` |
| 后端 tag | `tui-ringing-v2-interaction-causation-2026-09-24` |
| 后端 PR | `#324`（daemon 最小闭环）/ `#325`（control shape）/ `#326`（interaction 因果） |
| 上一版锚点 | `tui-ringing-v2-types-2026-09-24` @ `a43a8bc` |
| TUI rev | `feat/alpha1-v2-bootstrap@53cfea0` 加本轮改动 |
| daemon 构建 | `cargo build --bin qaqh-daemon`（锚点 worktree，18.6s） |
| daemon sha256 | `2eb40ddbef52e8e052bf40a84b6347290eaef77596ef6f18486997243c17b13c` |
| 锚点 worktree | `/home/qaqtamsy/项目/qaqh-backend-anchor`（detached `b77c251`） |

`b77c251` 在 `a43a8bc` 之上补齐了 daemon 侧
`open -> bootstrap -> since_cursor subscribe -> replay -> live` 的最小闭环，并把
control bootstrap 的形状对齐到 TUI reducer 的消费面（`#325`）。

## 2. 为什么必须换锚点

`a43a8bc` 只有 wire/client 类型，daemon 侧 `/ringing/v2` 还不存在，所以本仓
handoff（`2026-09-24-alpha1-v2-bootstrap-handoff.md` §4.1）把「等 P0-3」列为阻塞。
后端已用 `#324` / `#325` / `#326` 落地并另发冻结 tag；`a43a8bc` 停在旧 tag 上，
继续钉它等于**本机 anchor 与 CI pin 描述的不是同一个后端**（本机 worktree 已被
后端侧切到 `b77c251`，CI pin 还写 `a43a8bc`），本地红绿不再预测 CI。

## 3. 换锚点撞到的唯一破坏面

`cargo test` 在 `b77c251` 下红 1 条：

```text
app::ringing_v2::tests::client_bootstrap_adapter_maps_pending_driver_and_revision
panicked: client bootstrap: Error("missing field `activity`")
```

根因（对照 `a43a8bc` / `b77c251` 两个 rev 的 `qaqh-client/src/v2.rs`）：

| rev | `ClientV2ControlState` | 后果 |
|---|---|---|
| `a43a8bc` | `RingingV2ControlState` 的**别名**，该类型带 `#[serde(default)]` | 极简 fixture `{"interactions":…,"driver":…}` 能反序列化 |
| `b77c251`（`#325`） | **手写结构体**，容器上无 `#[serde(default)]`，`activity` / `tools` / `subagents` / `revision` / `last_fact_seq` 为非 `Option` | 这些字段在 wire 上变成**必填** |

daemon 侧本来就逐字段发全量（`axum_impl/v2.rs` 的 `V2ControlState` 与 client
结构体逐字段对应），所以**这不是 daemon 少发字段**，是本仓测试 fixture 手写得太省。

反证：把 override 临时切回 `a43a8bc`，同一条用例 11/11 全绿；切到 `b77c251` 即红。
（临时 worktree 与 config 已删除/还原，验证后工作区干净。）

### 3.1 修法

`src/app/ringing_v2.rs` 的该用例不再手列 control / conversation / tool 三个频道的
基线字段，改为用客户端自己的 `Default` 生成基线再覆写本用例关心的
`interactions` / `driver`：

```rust
let mut control_state =
    serde_json::to_value(ClientV2ControlState::default()).expect("control baseline");
control_state["interactions"] = serde_json::json!([…]);
control_state["driver"] = serde_json::json!({…});
```

这样后端再往快照里加必填字段时不用回来补 fixture，而整条 `serde_json::from_value`
反序列化路径仍然被走一遍（用例断言不变：`log_id` / `state_revision` 取三频道最大
值 / pending interaction kind / driver epoch）。

**没有**采取的另一条路是让后端给 `ClientV2ControlState` 补 `#[serde(default)]`
恢复 `#325` 之前的容忍度——那需要跨仓改动 + 新 tag，本仓先用不依赖后端的方式收口。

## 4. 验证证据

本仓门禁（全部在 `b77c251` 锚点下）：

```text
cargo test --all-targets -- --test-threads=1      348 passed / 0 failed / 9 ignored
cargo clippy --all-targets -- -D warnings         PASS
cargo fmt -- --check                              PASS
scripts/static-gates.sh                           PASS（G1–G4）
scripts/perf-gate.sh                              PASS
scripts/tests/ci-linux-parse-test.sh              15 passed / 0 failed
scripts/smoke-tui.sh                              PASS（默认 Agent View）
TUI_ARGS=--v1 scripts/smoke-tui.sh                PASS
```

性能门禁读数（阈值见 `src/app/render/bench.rs`）：

```text
version 未变同步: 882ns（<1ms）        live delta/帧: 10.247µs（<2ms）
440 回合常驻:     542 KB（<8MB）       440 回合 replay: 126.8ms（<3s）
块数线性度:       3.996（3.5–4.5）
```

真实 daemon（锚点 worktree 重建的 `b77c251` daemon）：

```text
e2e-v2-faults.sh       none / lagged / gap / ack-delay / ack-hang / session-404   6/6 PASS
e2e-v2-interactions.sh permission / ask / plan / pager / permission-hang /
                       ask-hang / permission-deny / ask-dismiss / plan-reject    9/9 PASS
```

复现命令：

```bash
cd /home/qaqtamsy/项目/qaqh-backend-anchor && cargo build --bin qaqh-daemon
cd /home/qaqtamsy/项目/qaqh-tui-app && cargo build --bin qaqh-tui
for m in none lagged gap ack-delay ack-hang session-404; do MODE=$m bash scripts/e2e-v2-faults.sh; done
for m in permission ask plan pager permission-hang ask-hang permission-deny ask-dismiss plan-reject; do
  MODE=$m bash scripts/e2e-v2-interactions.sh
done
```

## 5. 结论与未覆盖

- 换锚点已完成：`scripts/ci-linux.sh::QAQH_BACKEND_REV` = `b77c251`，本机
  `.cargo/config.toml` override 指向同一 rev 的锚点 worktree，两者一致。
- `Cargo.lock` **无需刷新**：两个 rev 之间没有任何 `Cargo.toml` 变化（依赖图未变），
  `cargo check/test` 在 `--locked` 语义下未要求改写锁文件。
- 仍然**没有**接线的部分（本仓 handoff §5 的后续项，不在本切片）：
  - `RingingV2SessionModel` 仍是纯状态机，未挂入 `SessionState`；
  - interaction / driver 的 typed payload 内部匹配仍等后端 issue `#323`
    （`ControlDelta` 内部类型不可命名 / `DriverChanged` 未进 canonical /
    pending request 载荷未定）；
  - TUI 协议仍是 Ringing v1，`--v1` 回退路径保留。
