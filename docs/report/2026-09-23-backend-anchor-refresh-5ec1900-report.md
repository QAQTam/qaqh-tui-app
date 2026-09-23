# 后端锚点刷新报告（2026-09-23）

> 状态：**已落地**（CI rev、计划/parity 文档、本机锚点机制三处已对齐）
> 触发：后端 P3 工作暂告一段落，机主确认可以刷新锚点
> 前次：[`2026-09-20-backend-anchor-refresh-report.md`](2026-09-20-backend-anchor-refresh-report.md)（`50d3dc1`）

## 0. 结论

| 项 | 值 |
|---|---|
| 新锚点 | `qaqh-backend @ 5ec1900d6c937b6ff927d8f65fcd37d465de7988`（`betav2` tip，2026-09-22） |
| 旧锚点（文档声称） | `50d3dc1dcfef`（2026-09-20） |
| 旧锚点（CI 实际生效） | `66539a0` —— **与文档不一致，见 §2** |
| TUI 侧代码改动 | 1 个文件 2 个字段（行为中性补齐），见 §3 |
| 门禁 | `check` / `test`（336 passed, 0 failed, 8 ignored） / `fmt` / `clippy -D warnings` 全绿 |

## 1. 为什么选 `5ec1900` 而不是更新或更旧的 rev

候选与"TUI 不改协议能否编译"的实测：

| rev | 说明 | `TodoItem` 在 `qaqh-client` | `ConversationSendMessage` 字段 | TUI 编译 |
|---|---|---|---|---|
| `50d3dc1` | 09-20 旧锚点 | ✅ | 4 个（旧） | ✅ 零改动 |
| `66539a0` | CI 实际钉的（更旧） | ✅ | 4 个（旧） | ✅ 零改动 |
| **`5ec1900`** | **后端 `betav2` tip** | ✅ | **6 个（+`message_id` +`input_purpose`）** | ⚠️ 差 2 个字段 |
| `be7ad6f` | 后端 P3 活跃分支 tip | ❌ 已删 | 6 个 | ❌ 需协议迁移（已按机主要求延后） |

`5ec1900` 是**能不改协议就让 TUI 编译的最新点**，且是后端的**集成分支**（不是会继续移动的
P3 feature 分支），因此选它。

## 2. 顺带修正：文档锚点 ≠ CI 锚点

`b612a05 chore(ci): 更新后端锚点到 50d3dc1` 对 `scripts/ci-linux.sh` 的改动，
在后续 merge（`f7c3ba4`）中被回退，导致：

- 计划 §4.5 / D6、parity D-07、M6.1 handoff、09-20 刷新报告 → 都写 `50d3dc1`；
- `scripts/ci-linux.sh:43` 实际生效 → `66539a0`。

`git log -L 43,43:scripts/ci-linux.sh` 可复现：该行最后一次变更是 `b01999f`（→ `66539a0`）。

**后果**：CI 跑的锚点与文档声称的不是同一个，"红了要能立刻归因给谁"这条纪律实际已失效。
本次一并纠正为 `5ec1900`。

## 3. 唯一的 API 变化与 TUI 侧处置

`45c6b63 feat(runtime): 两阶段 spawn 恢复扫描与消息去重 (Closes #260)`（后端 `betav2`）
给 `ConversationCommand::ConversationSendMessage` 加了两个字段：

```rust
/// Stable inter-agent message identity. User/UI messages may omit it
/// and fall back to the command id.
#[serde(default, skip_serializing_if = "Option::is_none")]
message_id: Option<String>,
/// Whether this message must trigger a turn or is queue-only.
#[serde(default)]
input_purpose: ConversationInputPurpose,   // #[default] = TriggerTurn
```

两者都是 `#[serde(default)]`（**wire 兼容**），且默认值就是改动前的行为，因此 TUI 侧是
**行为中性补齐**（`src/app/transcript_ops.rs`）：

- `message_id: None` = 回落到 `command_id`（用户/UI 消息的既定语义）；
- `input_purpose: Default::default()` = `TriggerTurn`，即"投递并触发回合"；
  `QueueOnly` 只服务子代理注入，UI 用不到。

**注意**：`ConversationInputPurpose` **未被 `qaqh-client` 再导出**，壳层无法命名该类型，
只能走 `Default::default()`。这与 `qaqh-client/src/types.rs` 开头自述的再导出纪律
（"壳层只认 `qaqh-client` 一个入口……缺一个名字，壳层就只能再抄一份镜像"）相悖，
**建议后端补上该 re-export**（不阻塞本次刷新）。

## 4. 本机锚点机制（本次新增）

此前 CI 有钉 rev 的逻辑，但脚本注释明写"**仅 CI —— 本机不动开发者工作树**"，
于是本地构建跟着开发者工作树跑（P3 分支上有未提交改动时会直接把 TUI 拖红）。
本次补上本机侧：

```bash
# 只读锚点 worktree（detached，不占分支、不影响既有工作树）
git -C ../qaqh-backend worktree add --detach ../qaqh-backend-anchor 5ec1900

# 本机 path override（.cargo/ 已入 .gitignore，不入库）
cat > .cargo/config.toml <<'EOF'
paths = ["/…/qaqh-backend-anchor/crates/qaqh-client",
         "/…/qaqh-backend-anchor/crates/qaqh-config-api"]
EOF
```

选 `paths` 而非 `[patch]`：`Cargo.toml` 里这两个是**路径依赖**，`[patch]` 只能覆盖
registry/git 源，覆盖不了 path 依赖。Cargo 会打印一条 `paths` 弃用提示，属预期噪声。

**换锚点的动作**（后端下次收口时）：改 `scripts/ci-linux.sh` 的 `QAQH_BACKEND_REV`
+ 重建 `../qaqh-backend-anchor` worktree + 同步 `.cargo/config.toml` 与计划/parity 文档。

## 5. 门禁实测（2026-09-23）

```text
cargo check --all-targets                         通过
cargo test --all-targets                          336 passed / 0 failed / 8 ignored
cargo fmt --check                                 通过
cargo clippy --all-targets -- -D warnings         通过
```

## 6. 遗留

- `ConversationInputPurpose` 未被 `qaqh-client` 再导出（见 §3），建议后端补；
- 后端 P3 活跃分支（`be7ad6f`）已删 `qaqh-client::TodoItem`，TUI 的
  `src/app/session.rs` 仍在用 —— **该迁移按机主要求延后**，P5 批次一并处理；
- 本机 `.cargo/config.toml` 含绝对路径，**不入库**（`.gitignore` 的 `/.cargo/`）；
  入库的只有 `scripts/ci-linux.sh` 的 rev 与文档锚点。

---

## 7. 后端回应（2026-09-23，同日）

按 [TUI 对后端的协作需求](../spec/2026-09-23-TUI对后端的协作需求-spec.md) §1 的锚点规格，
后端同日给出正式锚点：

| 项 | 值 | 核验 |
|---|---|---|
| tag | `tui-anchor-2026-09-23` | `git cat-file -t` = **`tag`**（annotated，非轻量）✓ |
| 指向 | `5ec1900d6c937b6ff927d8f65fcd37d465de7988` | `git rev-parse tui-anchor-2026-09-23^{commit}` 逐字相符 ✓ |
| 分支 | 后端 `betav2`（集成分支） | 满足规格 A1 ✓ |
| daemon | `../qaqh-backend-anchor/target/debug/qaqh-daemon` | sha256 `00642081e52a557cf332463090f964aa49e6b9a08b48e232974774cace490644`，与后端给的逐字相符 ✓ |
| 破坏性变更 | 无（`ConversationSendMessage` 两字段均 serde default） | 与 §3 的实测一致 ✓ |

**结论：TUI 侧的 pin 无需改动**——`scripts/ci-linux.sh` 的 rev 本来就是 `5ec1900`，
本次只是补上 tag 名作为可读引用。

### 7.1 `ConversationInputPurpose` 走独立小 PR

后端没有把该 re-export 挂在大 PR 上，而是基于 `betav2` 单开：

- PR **#289** `fix(client): re-export ConversationInputPurpose for TUI anchor`
- head `d60adea85c1581cbe286e2298ed931de18e2e112`，base `betav2`，**open / 未 merge**

**TUI 侧的处置：不锚 feature head**（违反规格 A1，且收益只是"能显式命名枚举"，
不阻塞任何功能）。按后端建议的流程走：

1. 等 #289 merge；
2. 后端**新开** tag `tui-anchor-2026-09-23-r2`（**不移动**旧 tag）；
3. TUI 升锚点，并把 `src/app/transcript_ops.rs` 的
   `input_purpose: Default::default()` 改为显式
   `ConversationInputPurpose::TriggerTurn`（该文件已留注释指向这条升级路径）。

## 8. 端到端验证（本次一并补上）

发现**同一类问题的第二个面**：9 个 e2e/smoke 脚本默认指向
`$REPO_ROOT/../qaqh-backend/target/debug/qaqh-daemon`，即**开发者正在移动的工作树**——
构建钉了锚点，但 e2e 没有，等于"门禁用的 daemon 和编译用的 client 不是同一个 rev"。

已全部改为默认吃锚点 worktree（`QAQH_BACKEND_ROOT` / `DAEMON` 覆盖保留）：

```text
scripts/e2e-lease-expiry.sh · e2e-v2-interactions.sh · e2e-v2-reconnect.sh
scripts/e2e-v2-resize-stress.sh · e2e-v2-session-switch.sh · e2e-v2-terminal-matrix.sh
scripts/smoke-tui.sh
```

顺带修掉两个更早的脚本（`e2e-restart.sh` / `e2e-session-list.sh`）——它们硬编码
`$HOME/Projects/...`，在非该目录布局的机器上直接找不到二进制。

实测（全部对着锚点 daemon）：

```text
scripts/smoke-tui.sh               ✓ 首帧渲染且未 panic（daemon pid=528581）
scripts/e2e-v2-session-switch.sh   RESULT: PASS
                                     [✓] 无 cursor-position timeout
                                     [✓] 两个会话标题均渲染
                                     [✓] 至少两次 scrollback purge
                                     [✓] alternate screen 进出
                                     [✓] 无 panic
```

即：**锚点 daemon 产物 → TUI 二进制 → v1 冒烟 + v2 端到端**全链路已跑通。

---

## 9. 第二次刷新：`tui-anchor-2026-09-23-p3`（2026-09-23 同日）

后端随后合并了 P3 与 TUI 契约两批（#288 / #289 / #290 / #291 / #292 / #293），
并按本仓 §7.1 约定的流程**新开 tag、不移动旧 tag**：

| 项 | 值 |
|---|---|
| 新 tag | `tui-anchor-2026-09-23-p3`（annotated，tag 对象 `1d06328`） |
| 指向 | `8dbe22e03239e95d19770dacd7b4dd0645ca6a7a`（后端 `betav2`） |
| 旧 tag | `tui-anchor-2026-09-23`（tag 对象 `65a94b6`）→ 仍是 `5ec1900`，未移动 ✓ |
| 本仓 pin | `scripts/ci-linux.sh` → `8dbe22e`；`.cargo/config.toml` 锚点 worktree 同步切到该 rev |
| daemon | 锚点 worktree 内重建，sha256 `740cf708d205b8e8a5c79a7490b2f2fe5ac31f38438f1d6fa09bf1fc365fae32` |

> 后端 issue 里把旧锚点写作 `65a94b6`——那是 **tag 对象**的 SHA（`git rev-parse <tag>`），
> 不是 commit SHA（`git rev-parse <tag>^{commit}` = `5ec1900`）。两者都对，只是口径不同；
> 本仓 `ci-linux.sh` 必须用 **commit SHA**（`prepare()` 拿 `rev-parse HEAD` 做字符串比对）。

### 9.1 本批后端交付（对应本仓协作需求的 §7 汇总表）

| 请求 | 后端交付 |
|---|---|
| 锚点规格 | `tui-anchor-2026-09-23-p3`（#288/#290/#292 全部纳入） |
| plan review 可控测试钩子 | `QAQH_TEST_PLAN_REVIEW=1`（#290） |
| 故障注入钩子 | `QAQH_TEST_INTERACTION_FAULT` / `SSE_TERMINATE` / `TIMELINE_GAP` / `COMMAND_ACK` / `SESSION_404_SEED`（#290） |
| `ConversationInputPurpose` 再导出 | #289 已合入 |
| 服务面 typed 变体（U-14） | #291 已合入 |
| 测试开关集中登记 | `docs/spec/2026-09-23-TUI契约测试钩子-spec.md` |
| typed todo 消费路径 | `docs/spec/2026-09-23-TUI-typed-todo消费路径-spec.md`（#292） |
| P5 wire 变更交底 | `docs/spec/2026-09-23-TUI-P5-wire变更交底-spec.md`（#293） |

### 9.2 TUI 侧落地（后端 issue #41 的三项待办）

**① `MODE=plan` 并入 `scripts/e2e-v2-interactions.sh`** ✅
MODE 闭集扩为 `permission|ask|plan|pager|permission-hang|ask-hang`；plan 模式在 daemon
env 里置 `QAQH_TEST_PLAN_REVIEW=1`，断言 modal 可见 + `a` 批准 + 无残留。

**② `permission-hang` / `ask-hang` 的超时 UI 终态** ✅ —— 这项**暴露了 v2 的一个真缺陷**：

- TUI 侧原本**没有交互应答的 ack 超时**（`spawn_api` 的注释还写着"今后如需统一超时……"），
  而 `respond_*` 是**乐观下架 modal** 的 → ack 永不返回时用户什么都看不到；
- 更关键：**v2 Agent View 完全不渲染 toast**（`grep toast src/ui/v2 src/terminal` 零命中），
  即所有命令失败 / 上传失败 / 超时在 Agent View 里**都不可见**。

处置：

- `app/mod.rs` 新增 `INTERACTION_ACK_TIMEOUT = 10s` 与
  `ApiCtx::send_interaction_command`（只作用于 permission/ask/plan 三类应答，
  不碰 compact/undo 等可能合法长耗时的命令）；
- 超时返回可读 `Err`，走**既有** `ActionResult::CommandAck` 错误分支 → toast；
- `terminal/agent.rs` 的 `status_line` 补上 toast 面，且 toast 存在时让位掉
  model/cwd/usage 等常驻项（保证瞬时提示一定画得出来）。

**③ 迁移到 `PlanReviewItem`** ✅ —— 纯改名，字段一字未动；
另修 `src/ui/modal.rs` 与 `src/ui/v2/modal.rs` 的 `{:?}` → `{}`
（`complexity` 是 `String`，`{:?}` 会渲染成带引号的 `"small"`）。
既然 #289 已合入，`input_purpose` 也从 `Default::default()` 改成显式的
`ConversationInputPurpose::TriggerTurn`。

### 9.3 实测（全部对着新锚点 daemon）

```text
cargo check --all-targets                   通过
cargo test --all-targets                    336 passed / 0 failed / 8 ignored
cargo fmt --check                           通过
cargo clippy --all-targets -- -D warnings   通过

MODE=permission      scripts/e2e-v2-interactions.sh   PASS
MODE=ask             scripts/e2e-v2-interactions.sh   PASS
MODE=plan            scripts/e2e-v2-interactions.sh   PASS（新增）
MODE=pager           scripts/e2e-v2-interactions.sh   PASS
MODE=permission-hang scripts/e2e-v2-interactions.sh   PASS（新增）
   [✓] permission modal visible / [✓] permission approved
   [✓] 应答超时提示可见 / [✓] 超时后 daemon 已被回收 / [✓] daemon 消失后仍能干净退出
MODE=ask-hang        scripts/e2e-v2-interactions.sh   PASS（新增）
scripts/e2e-v2-session-switch.sh                      RESULT: PASS
scripts/e2e-v2-reconnect.sh                           RESULT: PASS
```

### 9.4 仍未收口（M6.3 剩余）

- **plan review 的故障注入**（拒绝 / 超时 / 断流）——本批只做了 approve 正路径；
- **permission-deny / ask-dismiss** 两个一次性钩子尚未接进 harness；
- 其余故障钩子（`SSE_TERMINATE` / `TIMELINE_GAP` / `COMMAND_ACK` / `SESSION_404_SEED`）
  后端已备好但 TUI 侧**尚未接线**——这是 M6.3 接下来的主要工作面；
- 完整终端模拟器矩阵（9 个终端 × 5 维度）仍全 PENDING。
