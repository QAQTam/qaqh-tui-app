# TUI alpha1：协议跟读、Workspace 鼠标与开屏创建交接（2026-09-24）

> 状态：**main 干净、已推送；V2 fullscreen 为默认；plan/permission 协议已跟读；开屏创建失败/超时恢复已修复。**
>
> 仓库：`/home/qaqtamsy/项目/qaqh-tui-app`
>
> 后端锚点：`07cd172b7578e25e9d0682f584bca7644a3c2a01`（后端 main）
>
> 桌面二进制：`/home/qaqtamsy/桌面/qaqh-tui`
>
> 桌面二进制 sha256：`3742768a28e3754c7f99dc54d051595bb4d857260f4f55dd521a51f1ae644461`

## 0. 一句话

这一轮完成了三块 alpha1 前置工作：

1. V2 fullscreen 的 Workspace 统一鼠标交互；
2. 跟随后端 Ringing v2 协议修订：
   - bootstrap `PlanReview` wire 值统一为 `plan`；
   - permission 和 ask/plan 一样从 canonical `interaction.request` 下载正文；
   - 删除 permission 的 timeline 工具卡兜底；
3. 修复开屏页面输入首条消息后创建命令失败或超时时，界面一直卡在
   `正在创建会话…`、草稿悬空的问题。

## 1. 当前提交

最近提交（从旧到新）：

| Commit | 内容 |
|---|---|
| `223c90b` | Todo Workspace 打开与 todo 工具更新时刷新 dashboard |
| `4e5be96` | Workspace 统一鼠标 hit-test、hover/pressed、返回按钮 |
| `36c11ff` | permission canonical interaction body 消费；删除 timeline 兜底 |
| `8aaadb1` | CI 后端锚点切到 `07cd172`（plan wire + permission request ref） |
| `4ae393a` | 开屏创建失败/超时恢复，避免一直卡在 creating |

本 handoff 落库前的功能 tip：

```text
4ae393a fix(session): recover failed splash session creation
```

## 2. 当前 UI 基线

### 2.1 默认 shell

- 默认：V2 fullscreen。
- `--v2-agent`：fullscreen 兼容别名。
- `--v2-fullscreen`：显式 fullscreen。
- `--v1`：旧 v1 全屏兼容回退。
- `--v2-inline`：已退役，启动会报错。
- `QAQH_V2_INLINE`：已退役。

### 2.2 Workspace 鼠标

`src/app/mod.rs` 现在只有一套 Workspace 鼠标语义：

```rust
pub enum WorkspaceHit {
    SessionRow(usize),
    HistoryTurn(usize),
    TodoTask(usize),
    SettingsRow(usize),
    Back,
}
```

绘制和命中测试共用几何：

- Sessions 可见窗口；
- History 列表可见窗口；
- Todo 行范围与滚动窗口；
- footer `[ ← 返回 ]` 按钮。

行为：

- Sessions 行点击打开会话；
- History 行点击进入回合详情；
- Todo 行点击切换详情；
- Settings 行沿用原设置动作；
- 返回按钮按页面层级复用 Esc 语义；
- 滚轮在列表页移动选择，在 Todo/Subagent/History 详情滚动。

关键文件：

```text
src/ui/v2/workspace.rs
src/terminal/agent/mod.rs
src/app/overlay_ops.rs
src/app/mod.rs
```

## 3. Ringing v2 协议跟读

### 3.1 PlanReview wire

`ClientV2InteractionKind::PlanReview` 的 Rust 变体名保留，但 wire 值由
`qaqh-client` 统一为：

```json
"plan"
```

TUI 不再判断 `plan_review` 字符串，只匹配 typed enum variant：

```rust
ClientV2InteractionKind::PlanReview
```

对应回归：

```text
app::ringing_v2::tests::client_plan_review_variant_maps_to_internal_plan_review
```

### 3.2 Permission canonical body

现在 permission 和 ask/plan 一样：

- live `InteractionRequested`：消费 `request`；
- bootstrap `restore_pending_interaction`：消费 `interaction.request`；
- `Inline`：直接构造面板；
- `Ref`：下载 content store 正文后构造面板；
- permission 的 canonical `call_id` 随异步结果回传。

permission body 形状：

```json
{
  "kind": "permission",
  "tool_name": "exec",
  "action_summary": "cargo test",
  "reason": "...",
  "paths": ["..."],
  "category": "exec",
  "level": 3,
  "risk": "high",
  "consequence": "..."
}
```

构造入口：

```rust
PermissionPanel::from_interaction_body(tool_call_id, bytes)
```

已删除旧 timeline 兜底：

- `queue_permission_from_timeline`
- `SessionState::permission_panel_for`
- `TimelineModel::tool_card`
- 旧实时 `queue_permission` 覆盖路径

### 3.3 CI 锚点

`scripts/ci-linux.sh`：

```text
QAQH_BACKEND_REV=07cd172b7578e25e9d0682f584bca7644a3c2a01
```

本机 `.cargo/config.toml` 已切到对应本地 worktree：

```text
/home/qaqtamsy/项目/qaqh-backend-anchor-07cd172
```

注意：`.cargo/config.toml` 是本地忽略文件，不属于提交内容；新机器需要按
`scripts/ci-linux.sh` 的 rev 重新建锚点。

## 4. 开屏创建修复

### 4.1 现象

开屏输入框输入文字后按 Enter，界面可能一直显示：

```text
正在创建会话…
```

### 4.2 根因

两条失败路径原先没有闭环：

1. `SessionCreate` 的 transport error：
   - 只 toast；
   - 不撤销 `pending_creates`；
   - 状态一直停在 creating。
2. 等待 `SessionCreated` 超过 15 秒：
   - `handle_tick` 只静默删除 pending；
   - `pending_initial_prompt` 没有放回输入框；
   - 用户看到状态消失/草稿悬空，或者创建态长期残留。

### 4.3 修复

新增：

```rust
App::abort_pending_create(message)
```

行为：

- 清空 `pending_creates`；
- 开屏场景下把 `pending_initial_prompt` 放回 `draft_composer`；
- 有活动会话时把草稿放回该会话 composer；
- 给出可见错误 toast。

触发点：

- `ActionResult::CommandAck` 的 `Err(e)` 且 `seed.is_none()`；
- `handle_tick` 中 pending create 超过 15 秒。

正常成功路径不变：

```text
SessionCreated / SessionList 兜底
  -> open_session_tab
  -> transfer_pending_initial_prompt
  -> composer 中保留首条草稿
```

回归测试：

```text
app::tests::create_transport_error_restores_draft_and_clears_pending
app::tests::create_timeout_restores_draft_and_clears_pending
```

## 5. 验证证据

在 backend main `07cd172` 下验证：

```text
cargo test --all-targets -- --test-threads=1
  407 passed / 0 failed / 9 ignored

cargo clippy --all-targets -- -D warnings
  PASS

scripts/static-gates.sh
  PASS

scripts/perf-gate.sh
  PASS
```

真机 e2e：

```text
MODE=plan       scripts/e2e-v2-interactions.sh  PASS
MODE=permission scripts/e2e-v2-interactions.sh  PASS
scripts/e2e-new-session.sh                       PASS
```

开屏输入路径额外用真 PTY 验证：

```text
输入：hello-splash
Enter
最终：❯ hello-splash + ● ready，无残留“正在创建会话”
```

## 6. 当前缺口 / 下一步

按 V2 fullscreen 冲刺顺序：

1. 工具卡点击展开；
2. thinking 历史展开；
3. 滚动条拖拽；
4. 文本选择 + OSC52 复制；
5. `Ctrl+F` 搜索；
6. Todo 页面全交互；
7. retry / fork 后端语义接线；
8. 终端矩阵、真模型、长会话、resize 收口；
9. 最后删除 v1 / inline 兼容代码。

## 7. 接手注意

- 不要恢复 `plan_review` 字符串判断；TUI 只依赖 typed enum。
- permission 不再从 timeline 工具卡取详情；不要重新引入该兜底。
- `InteractionBody` 异步回调必须携带 permission 的 `call_id`，否则无法构造
  `PermissionPanel`。
- 开屏首条消息当前语义是“创建会话并把草稿带入 composer”，**不会自动发送**；
  用户可以在真实 composer 里继续编辑后按 Enter。
- 不要删除 `pending_creates` 超时/错误恢复：这是开屏创建卡死的防回退点。
- 本机当前后端锚点 worktree 是
  `/home/qaqtamsy/项目/qaqh-backend-anchor-07cd172`，不要误用旧
  `qaqh-backend-anchor` 的 `c2a5d40`。
- 桌面 debug 二进制已更新；测试优先使用
  `/home/qaqtamsy/桌面/启动-qaqh-tui.sh`。

## 8. 常用命令

```bash
cargo fmt -p qaqh-tui
cargo clippy --all-targets -- -D warnings
cargo test --all-targets -- --test-threads=1
scripts/static-gates.sh
scripts/perf-gate.sh

QAQH_BACKEND_ROOT=/home/qaqtamsy/项目/qaqh-backend \
DAEMON=/home/qaqtamsy/项目/qaqh-backend/target/debug/qaqh-daemon \
TUI=/home/qaqtamsy/项目/qaqh-tui-app/target/debug/qaqh-tui \
MODE=permission bash scripts/e2e-v2-interactions.sh

QAQH_BACKEND_ROOT=/home/qaqtamsy/项目/qaqh-backend \
DAEMON=/home/qaqtamsy/项目/qaqh-backend/target/debug/qaqh-daemon \
TUI=/home/qaqtamsy/项目/qaqh-tui-app/target/debug/qaqh-tui \
RUN_SECS=18 bash scripts/e2e-new-session.sh
```
