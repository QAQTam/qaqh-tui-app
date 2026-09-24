# TUI alpha1 / Ringing v2 bootstrap 交接（2026-09-24）

> 状态：**PR #49 open；本地全量与真实 daemon 全绿；CI 在 Prepare 因组织 CPU 配额失败。**
> 分支：`feat/alpha1-v2-bootstrap`
> Tip：`414de6c`
> 后端锚点：`tui-ringing-v2-types-2026-09-24` @
> `a43a8bcfb01e4cf0f97154f942d874f60cd54aa6`
> 详细 reducer 交接：`docs/handoff/2026-09-24-ringing-v2-reducer-handoff.md`
> 真实 daemon 验证：`docs/report/2026-09-24-backend-main-anchor-verification-report.md`

## 0. 一句话

TUI 已完成 alpha1 的第一批 v2 起步工作：升级到后端 v2 最小类型锚点、默认切
V2 Agent View（协议仍 v1）、实现 Ringing v2 SessionModel 纯 reducer 与
`qaqh-client` typed bootstrap/event/reset 适配。下一步等后端 P0-3
daemon `/ringing/v2` 与 issue #323 的 typed interaction/driver 消费面。

## 1. 当前提交

| Commit | 内容 |
|---|---|
| `e82d154` | anchor 升级到 `a43a8bc`，更新 CI pin / Cargo.lock / 验证报告 |
| `dbe68be` | alpha1 默认 V2 Agent View；`--v1` 保留硬回退 |
| `41355e6` | Ringing v2 SessionModel reducer + typed bootstrap/event/reset 适配 |
| `b9d6a70` | 同 epoch 不同 driver holder 判为 `Conflict` |
| `414de6c` | reducer 专项 handoff |

PR：

```text
https://cnb.cool/QAQ-Harness/qaqh-tui-app/-/pulls/49
```

CI：

```text
cnb-qih-1k37hib51-001
Prepare: error
Root Group's events CPU core-hours are insufficient for pre-freezing
```

CI 未进入代码检查；本地已覆盖下列验证。

## 2. 已完成

### 2.1 Anchor

- `scripts/ci-linux.sh::QAQH_BACKEND_REV` 已切到 `a43a8bc`。
- 本机 `../qaqh-backend-anchor` 已 detached 到同一 tag。
- `Cargo.lock` 已按新路径依赖图刷新。
- `scripts/e2e-v2-faults.sh` 的旧锚点“预期红”注记已更新。

### 2.2 M7 UI 默认

- `qaqh-tui` alpha1 默认进入 V2 Agent View。
- `--v1` 仍强制回退 v1 全屏。
- `--v2-agent` / `QAQH_V2_AGENT` 保留为显式兼容入口。
- **协议仍是 Ringing v1**；v1 -> v2 协议切换继续等 daemon P0-3。
- `scripts/smoke-tui.sh` 已改成真 PTY 驱动：
  - 响应 inline viewport 的 `ESC[6n` 查询；
  - 支持 `TUI_ARGS`；
  - 覆盖默认 Agent View 与 `--v1` 两条真实启动路径。

### 2.3 Ringing v2 reducer

文件：`src/app/ringing_v2.rs`

已锁不变量：

1. reliable 严格按 `(fact_seq, projection_index)` 前进；
2. exact repeat `Duplicate`，旧位置 `Stale`；
3. `server_epoch` / `log_id` 不匹配拒绝；
4. replaceable 只更新 revision，不推进 cursor；
5. ephemeral 不改变持久态；
6. reset 只标记 pending，保留旧模型；
7. 新 bootstrap 验证后原子替换；
8. interaction 按 `interaction_id` 幂等，终态不可重开；
9. driver epoch 单调；旧 epoch `Stale`，同 epoch 不同 holder `Conflict`。

### 2.4 Typed adapter

已提供：

```text
bootstrap_from_client(&ClientV2Bootstrap)
event_meta_from_client(&ClientV2Event)
payload_family(&ClientV2Event)
reset_from_client(&ClientV2Reset)
```

适配只经 `qaqh-client`，没有直接依赖 `qaqh-ringing` / `qaqh-domain` /
`qaqh-session`。payload 当前只做顶层 family 分类；interaction/driver 内部
typed 匹配等后端 #323。

## 3. 验证证据

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

后端 daemon：

```text
rev: a43a8bcfb01e4cf0f97154f942d874f60cd54aa6
sha256: 40de1873b92b0403682fa60b10ad6e934f9be23fece7e802087e8aff971d9f9b
```

## 4. 当前阻塞

### 4.1 后端 P0-3

daemon `/ringing/v2` 尚未实现：

- open / bootstrap / since_cursor 原子 subscribe；
- reliable replay -> live；
- ResetRequired -> rebaseline；
- interaction replay / driver claim/release 的 daemon 接线。

### 4.2 后端 #323

`QAQ-Harness/qaqh-backend#323` 仍 open：

- `ClientV2Payload` 内部 `ControlDelta` 类型不可从 `qaqh-client` 命名；
- 缺 reliable `DriverChanged` canonical projection；
- pending interaction 的 request 载荷或 replay 保证未明确。

这三项决定 TUI 能否把 interaction/driver typed payload 接进 reducer。

### 4.3 CI 配额

PR #49 的 CI 红是组织 CPU core-hours 不足，不是代码失败。配额恢复后重跑即可。

## 5. 下一步

1. 后端完成 P0-3 daemon 最小闭环，并发布不可移动 tag。
2. 后端完成 #323 的 typed payload / DriverChanged / pending request。
3. TUI 把 `RingingV2SessionModel` 挂入 `SessionState`：
   - open -> bootstrap -> subscribe -> replay -> live；
   - reset 时先完成新 bootstrap 验证，再原子替换模型；
   - interaction/driver payload 经 typed adapter 进入 reducer。
4. 补 v2 fixture，覆盖 `V2-C1..C7` / `V2-R1..R4` / `V2-D1..D3`。
5. 协议切换完成后：
   - 删 `src/app/ringing_v2.rs` 顶部临时的 `#![allow(dead_code)]`；
   - 把默认协议从 v1 切到 v2；
   - 保留 `--v1` 回退路径到兼容窗口结束。

## 6. 接手注意

- 不要回退 `scripts/smoke-tui.sh` 到 `script` 驱动：默认 Agent View 需要 PTY
  响应 `ESC[6n`，否则会把正常初始化误报成 panic。
- 不要让 TUI 直接依赖 `qaqh-ringing` / `qaqh-domain` / `qaqh-session`；静态门禁
  G1 会红。
- reducer 目前是纯状态机，尚未接入生产事件循环；不要把它误当成协议已切换。
- 后端主工作树当前在 `feat/ringing-v2-daemon-min-loop` 且有未提交改动，TUI 侧
  没有修改该工作树。
