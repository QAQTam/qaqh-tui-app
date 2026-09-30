# TUI 协议权威化迁移报告（2026-09-30）

> 代码基线：`qaqh-tui` main @ `53c4c6e`（`2.0.0-alpha3`），工作树含本次改动
> 后端锚点：`qaqh-backend` main @ `1154ec7ec05eddb145f0e22b6812314519769722`（`2.0.0-alpha4`）
> 来源：issue「根据后端协议侧改动，对 TUI 侧进行跳转，消灭 TUI 自身维护协议的
> 局限性，统一使用后端接口」

---

## 1. 结论先行

TUI 侧的**第二份协议**已经删干净：`src/app/ringing_v2.rs` 曾自带六份手抄协议镜像
与三个适配器，现在一行不剩，全部改为 `use qaqh_client::*`。同时接上了后端本轮新增
的两项协议面（信封 `ts_ms`、`CompactionApplied`），并把内部身份命名统一为
`session_id`（924 处）。

三项**已发生过的真实漂移**（其中一项本次直接编译失败）是这次迁移的理由，不是推测：

| # | 漂移 | 暴露方式 |
|---|---|---|
| 1 | 后端 bootstrap 把 `seed` 改名 `session_id`（`refactor(identity)`），镜像与手抄 fixture 仍按旧键读 | **静默**：`ringing_v2.rs` 的 `client_bootstrap` fixture 用 `"seed"`，只有 `#[cfg(test)]` 读者 |
| 2 | 信封新增 `ts_ms`（beta-readiness W1/C3，`0607efa`） | **编译失败**：`mod.rs` 两处手抄 `ClientV2Event` 字面量缺字段，TUI 对后端 HEAD 根本构建不过 |
| 3 | 镜像 `PendingInteraction` 漏了权威类型的 `request`（modal 正文） | **静默**：零生产读者发现 |

迁移前的基线事实：`cargo check --all-targets` 对后端 HEAD **失败**（2 处 E0063
缺 `ts_ms`）；CI 锚点 `2f362e0`（alpha2）落后 HEAD 91 个提交。

---

## 2. 后端侧改动的 TUI 相关面

| 后端提交 | 协议面 | TUI 原有处置 |
|---|---|---|
| `0607efa` W1/C3 | `RingingV2EventEnvelope.ts_ms`（源 fact 墙钟，`#[serde(default)]`） | 无字段镜像 → 静默丢弃；手抄 fixture 编译失败 |
| `58c9c76` W3/D10 | `ConversationDelta::CompactionApplied` fact 产生侧 | `handle_conversation_delta` 里 `=> {}` **直接丢弃** |
| `42820ff` D4 | 归档深翻页回填 `turn_index` | 已消费 `before_index`（此前已迁移），仅注释仍写 `before_turn` |
| `2e064d0` 等 | canonical `session_id`（legacy `seed` 查询键退场） | TUI 内部仍 924 处 `seed` 命名 |
| `f55825c`/`0b9b595` hub-fact-bus 3d/3.2 | v1 事件总线与 v1 信封删除 | TUI 已是 v2-only，无影响（仅文档仍写 `/ringing/v1/*`） |
| `a015e0b` | 网关透传 `since_cursor` | 由 `qaqh-client` 承担 |

---

## 3. 做了什么

### 3.1 删除协议镜像（核心）

`src/app/ringing_v2.rs` 重写。删除清单：

| 删除 | 替换为 |
|---|---|
| `enum Delivery` | `qaqh_client::ClientV2Delivery` |
| `enum InteractionKind` | `qaqh_client::ClientV2InteractionKind` |
| `enum ResetReason`（15 变体） | `qaqh_client::ClientV2ResetReason` + 穷尽 `match` 的只读策略 |
| `struct DriverState` | `qaqh_client::ClientV2DriverState` |
| `struct PendingInteraction`（漏 `request`） | **删除**；状态机只记交互**身份**（`BTreeSet<String>`），正文由 typed payload 直达 UI |
| `struct BootstrapSnapshot` | **删除**；`apply_bootstrap(&ClientV2Bootstrap)` |
| `struct EventMeta` + `event_meta_from_client` | **删除**；`apply_event(&ClientV2Event)` 直接读权威信封 |
| `struct ResetSignal` + `reset_from_client` | **删除**；`begin_reset(&ClientV2Reset)` 存权威原帧 |
| `bootstrap_from_client` | **删除**；用上游 `log_id()` / `snapshot_cursor.decode_snapshot()` |
| `ApplyOutcome::Malformed` | `ApplyOutcome::Incomplete`（语义收窄：只表示「读不到值无法推进」，非第二套校验） |
| `BootstrapOutcome::SeedMismatch` | `BootstrapOutcome::SessionMismatch`（对齐权威键名） |

只读策略写成**穷尽 `match`**（不是 `matches!`）：后端新增 reset reason 时编译失败，
逼一次显式决策，而不是默默归入「可写」。

保留的只有 TUI 自己的会话级时序不变量：epoch/log/cursor 单调性、重复与过期事件
丢弃、交互身份「第一答复胜出」、driver seat 按 epoch 单调交接。

### 3.2 接上后端新增协议面

**`ts_ms`（权威源 fact 墙钟）**

- `Turn.started_at_ms: Option<u64>`：由 v2 信封回填（timeline wire **没有**时间字段）；
- 信封时间可能早于 timeline 条目（两条独立 SSE）→ `TimelineModel.pending_turn_ms`
  暂存，`TurnOpened` 物化时兑现；
- `TranscriptBlock.at_ms` → 用户回合头渲染本地 `MM-DD HH:MM`；
- **`None` 就什么都不画**：不用本地时钟兜底（那会让「服务端时间」与「猜的时间」
  长得一模一样）；
- 快照整体替换清除暂存（权威全量不跨代）。

**`CompactionApplied`（W3/D10）**

- `TimelineModel.compaction_marks: Vec<CompactionMark>`，按 `context_revision` 幂等；
- 锚定「事件到达时窗口最后一个回合**之后**」（与 webui W3 同口径），渲染
  「── 此前已压缩 ──」`BlockKind::System` 块；
- 锚点回合被淘汰（`cap_turns` / 深翻页）→ 分隔条**顶到最前**而不是消失；
- 快照重载清除锚。

### 3.3 身份命名统一为 `session_id`

924 处 `seed` 命名 → `session_id`（`SessionState` / `StreamKey` / `RuntimeMsg` /
`set_tracked_session_ids` / `active_session_id` / 测试函数名……）。字符串取值同步改为
`session-…` 形状；导出路径夹具改用规范 UUID-ish 身份（首 8 位纯十六进制），
让「前 8 位」断言不被分隔符污染。

**故意保留两处 legacy 拼写**（并注明理由）：`reset_from_legacy_seed_key()` 及其
`"seed"` 键——它测的就是「旧键名不再被接受」，改名会让断言失去意义；以及
`mod.rs` 里指「当时的 wire 键名」的历史注释。全仓 `[Ss]eed` 只剩这两处。

### 3.4 测试夹具权威化（防复发的关键）

新增 `src/app/v2_fixtures.rs`（`#[cfg(test)]`）：信封 / 快照 / reset / payload
一律 `serde_json::from_value::<qaqh_client 权威类型>`，基线由上游 `Default` 与
编码器生成。于是：

- 后端加**必填**字段 → 反序列化**当场报错**；
- 后端加**可选**字段 → 走上游 `#[serde(default)]`，正是它声明的语义。

删掉的手抄点：`mod.rs` 两处 `ClientV2Event` 字面量、一处手写 bootstrap JSON
（还带着 legacy `"seed"` 键）、`v2_bootstrap` 的手写 JSON、`ringing_v2.rs` 的
`client_bootstrap` 手写 JSON。

### 3.5 门禁（把删除变成可执行契约）

`scripts/static-gates.sh`：

- **G2 白名单**新增 `src/app/v2_fixtures.rs`，理由写明「纯 `#[cfg(test)]`，本仓唯一
  允许权威类型 ↔ JSON 互转处」；
- **新增 G5**：`src/app/ringing_v2.rs` 不得再声明 `Delivery` / `InteractionKind` /
  `ResetReason` / `PendingInteraction` / `DriverState` / `BootstrapSnapshot` /
  `EventMeta` / `ResetSignal` 这些协议词汇（`enum|struct|type` 声明形式）。
  已做负向对照：注入 `pub enum Delivery {` 等三行 → 门禁按预期变红。

### 3.6 文档与锚点

- `README.md`：架构段与协议纪律段**重写**（原文仍描述早已删除的 `src/protocol/`
  7 文件镜像层、`src/transport/`、v1 三频道 SSE、`/ringing/v1/*`、`protocol::methods`、
  `before_turn`、400 回合上限——与代码相反的文档断言）；新增「协议面」「协议权威化」
  两节；
- `scripts/ci-linux.sh`：`QAQH_BACKEND_REV` → `1154ec7…`，并写明本次迁移依赖的
  四项后端面（`ts_ms` / `CompactionApplied` / canonical `session_id` /
  `before_index`+`turn_index`）。

---

## 4. 验证

| 项 | 结果 |
|---|---|
| `cargo test --all-targets` | **336 passed / 0 failed**（迁移前对 HEAD 为编译失败） |
| `cargo clippy --all-targets -- -D warnings` | 通过（0 warning） |
| `cargo fmt --check` | 通过 |
| `scripts/static-gates.sh` | 通过（G1–G5 全绿）+ G5 负向对照成立 |
| 真机 open 握手（隔离 home + 隔离 data root，真 daemon `2.0.0-alpha4`） | `qaqh-tui doctor` exit 0，`open: session=… epoch=… lease_ttl=30000ms renew=10000ms` |
| 后端 `cargo test -p qaqh-ringing` | **21 passed / 0 failed**（含 v2 信封 `ts_ms` 断言） |
| 后端 `cargo test -p qaqh-client` | 48 passed / **2 failed** —— 失败项是 `discovery::tests` 里 `Command::new("sh")` 的两条判活测试，**Windows 无 `sh`**（后端 `docs/plan-beta-readiness.md` G4 已把同类环境失败定性），与本次改动无关：后端仓 `git status` 全程干净，本仓未改后端一行 |

新增回归锁（节选）：

- `reset_read_only_policy_covers_every_authoritative_reason`（穷尽映射，加变体即编译红）；
- `reset_frame_requires_authoritative_session_id`（legacy `seed` 键必须解析失败）；
- `pending_set_tracks_identity_only`（断言正文在权威 payload 上、状态机不复制）；
- `bootstrap_for_another_session_is_rejected`；
- `envelope_ts_ms_is_consumed_from_the_authoritative_source`；
- `authoritative_wall_clock_comes_from_the_envelope_and_is_never_faked`；
- `snapshot_rebaseline_clears_pending_time_and_compaction_anchors`；
- `compaction_anchor_follows_the_last_turn_and_is_idempotent`；
- `compaction_mark_splices_after_its_anchor_turn` / `..._with_evicted_anchor_goes_to_the_top`。

---

## 5. 未完成 / 需要人工动作

**1. 后端 11 个提交尚未推送（阻塞 CI）**

`1154ec7` 及以下 11 个提交目前只在后端**本地** `main` 上，`origin/main` 仍是
`7bb83f0`。CNB CI 从 `origin` clone，因此：

- 钉旧 rev（`2f362e0`）→ TUI 编译失败，但错误全部指向 TUI 代码，**归因错误**；
- 钉 `origin/main`（`7bb83f0`）→ 同样编译失败（无 `ts_ms`）；
- 本仓选择钉 `1154ec7` → 失败信息是「checkout 不到该 rev」，**指向真正缺的东西**。

后端推送 `main` 后 CI 无需改动即可转绿。这是本次唯一的外部依赖。

**2. 真 PTY 端到端未在本机跑**

`scripts/e2e-*.sh` 依赖 POSIX `pty`（`scripts/lib/pty-driver.py`），本机为 Windows，
只能跑非交互的 `doctor`。`ts_ms` / 压缩分隔的**上屏**证据目前来自单元测试
（adapter + timeline model），真机渲染需在 Linux 上跑
`scripts/e2e-alpha1-basic.sh` 一类脚本确认。

**3. 压缩分隔是位置锚，不是事实锚**

与 webui 同口径：锚在「事件到达时刻的最后一个回合之后」。按
`replaces_through_fact_seq` 反查历史位置需要后端提供可映射的 timeline 事实，
暂不可得（已记入 README「已知边界」）。

---

## 6. 影响面

23 个文件（含 1 个新增、1 个脚本、1 个 README）：
`src/app/{ringing_v2,mod,timeline_model,session,session_ops,team,subagent,interaction,
composer_ops,export,overlay_ops,transcript_ops,settings,v2_fixtures}.rs`、
`src/{runtime,ui/v2/{adapter,transcript,modal,route,workspace},terminal/agent/{mod,fullscreen}}.rs`、
`scripts/{static-gates.sh,ci-linux.sh}`、`README.md`、`Cargo.lock`（path 依赖 alpha3→alpha4）。

无 wire 行为变更：命令/事件/cursor/reset 语义完全由 `qaqh-client` 决定；本次只把
TUI 的**消费面**从自维护镜像换成权威类型，并补上此前被丢弃的两项协议事实。
