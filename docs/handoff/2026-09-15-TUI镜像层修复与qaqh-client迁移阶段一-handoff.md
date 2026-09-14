# TUI 镜像层修复（T-02/T-04/T-07）与迁移 `qaqh-client` 阶段一分析 handoff（2026-09-15）

## 交接摘要

本批在**不迁移**的前提下修掉 TUI 侧三个缺陷，建立 TUI 缺陷清单并与后端权威登记册对接，
随后完成 T-01（迁移 `qaqh-client`）**阶段一**的可行性核实。

- 代码：`cargo test` **166 passed / 0 failed**；`cargo clippy --all-targets` **零 warning**；`rustfmt --check` 干净
- 三个修复都有**变异验证**（证伪过恒真测试，见 §1）
- 迁移阶段一的结论：**前置阻塞已解除，可以开工**；阶段二仍被后端枚举缺口阻塞（§2）
- 缺陷清单：`docs/buglist/2026-09-15-TUI已知缺陷跟踪-buglist.md`（与后端 `qaqh-backend/docs/buglist/` 的映射见其 §8）

## 1. 已完成的修复

| 缺陷 | 位置 | 回归测试 | 变异验证（实测） |
|---|---|---|---|
| **T-02** 流首 BOM 吞首帧 | `transport/sse.rs:13/34/59-69`（`BOM` 常量 + `bom_checked` + 跨 chunk 等待分支） | `leading_bom_is_stripped_and_first_frame_survives`、`leading_bom_does_not_break_data_only_frame`、`bom_split_across_chunks_is_handled`、`bom_is_stripped_only_at_stream_start` | 剥离动作置空 → **3 红** |
| **T-04** D-3 无回归锁 | `runtime.rs:339` `stream_rebuild`（`:320` 枚举），两条流共用（`:480`/`:703`） | 5 个（`generation_bump_rebuilds_stream_and_keeps_cursor` 等） | 删 generation 分支 → **2 红** |
| **T-07** `TurnOpened` 不镜像后端原地 reopen | `app/timeline_model.rs`：`Turn.sealed` 字段（`:249`/`:266`/`:277`）、reopen 分支（`:391`）、`TurnSealed` 置位（`:543`） | `sealed_turn_is_reopened_in_place`、`running_turn_duplicate_opened_keeps_content` | 双向：reopen 永不命中 → 1 红；所有重复都 reopen → 2 红 |

**T-07 补充说明**：后端在 daemon 重启后用「原地 reopen」复用已 sealed 的 `turn_id`
（`qaqh-runtime/src/timeline.rs:208-244`）。判据是 `turn.sealed`，**不是** `state != Running`——
故本修复把线协议里一直存在、但 `from_wire` 丢弃的 `sealed` 字段补回应用层 `Turn`。

> ⚠️ 初版曾写过一个「reopen 后 `fragment_seq=0` 被接受」的测试，变异①下**仍然通过**：
> B2（重复 `BlockOpened` 整块替换）本就会重置 `last_fragment`，该测试恒真，已删除。
> 记在这里是为了避免后人再加一个同样恒真的锁。

## 2. 迁移 `qaqh-client`：阶段一核实（在 2026-09-14 评估之上）

基线：评估文档 `docs/plan/2026-09-14-TUI传输层迁移qaqh-client-plan.md`（结论「建议迁移，
但不建议全量替换；分两阶段」）。本节只记录**本次核实到的变化与新增事实**，不重复其内容。

> ⚠️ **与评估的两处分歧（先读这条）**：
> ① 评估把迁移切成两阶段，并建议阶段一用选项 (c)（临时薄适配器）兜底服务面。
> 本次核实后**不采纳**——缺口只有 2 个方法（2.4），故改为**三刀切**（2.5），
> 阶段一完全不碰服务面，**一个临时适配器都不需要**。
> ② 评估的规模指标（`transport/` + `runtime.rs` → ≤ 400 行）**不成立**：`http.rs` 里有约
> 280 行是服务面赖以工作的客户端核心，删不掉。修正目标见 2.5 验收。

### 2.1 已变化的前提

| 项 | 09-14 评估 | 本次核实（2026-09-15） |
|---|---|---|
| 后端编译（评估排期里的 P0 前置） | `rebase/76` HEAD 编译失败（11 error） | ✅ **已解除**：`cargo check -p qaqh-runtime --lib` 零 error |
| 后端基线 | `c1e5a3d`（rebase/76） | `1c92413`，且与**在跑 daemon 的 `build_id` 一致** |
| TUI 镜像层规模 | transport+runtime+protocol ≈ 4336 | **4546**（transport 1178 + protocol 2419 + runtime 949），已增长 |
| 对照 crate | qaqh-client ≈ 3091 | 3091（未变） |

### 2.2 阶段一的目标 API（本次源码核实）

| TUI 现状 | `qaqh-client` 对应物 |
|---|---|
| `runtime.rs:202` `supervisor()`（open + 续租循环） | `RingingSession::{open, run_renewal}`（`session.rs:68`/`:145`） |
| `runtime.rs` 三条频道流 + `ConnEvent` | `ClientHandlers::{on_batch, on_status, on_reset}`（`client.rs:30-39`，`Arc<dyn Fn>` 回调） |
| `runtime.rs` per-seed timeline 任务表 | `Client::activate_timeline(seed)`（`client.rs:558`）+ `on_timeline_entry`/`on_timeline_status`/`on_timeline_snapshot` |
| `runtime.rs:193` `refresh_credentials()` | 内建（`session.rs` 失败计数 → 重新 open） |
| `runtime.rs:339` `stream_rebuild`（本批 T-04 的锁） | `RingingSession::session_ctx_rx()`（`session.rs:123`）——**(epoch, cs) 变更广播，D-3 的机制在 client 里是原生的** |
| `transport/http.rs` 401 三态 `classify` | `error.rs` `ClientError` 分类 |
| `transport/sse.rs` `SseDecoder` | `sse_decoder.rs`（已含 BOM 剥离） |
| `transport/discovery.rs` | `discovery.rs` `DiscoveryExt` / `ensure_daemon_running` |
| `main.rs` 自建 tokio runtime | `runtime_handle()`（`client.rs:744`）——**二选一，勿双运行时** |
| `transport/http.rs` `service(method, params)` 泛型口 | `Client::{query(QueryRequest), action(ActionRequest)}`（**封闭枚举，见 2.4**） |

### 2.3 依赖差异（09-14 评估标「需核对」，本次核实：**有 4 处需处理**）

| 依赖 | TUI（`Cargo.toml`） | `qaqh-client` | 后果 / 处置 |
|---|---|---|---|
| `reqwest` | `0.13`，features `json/stream/multipart/`**`native-tls`** | `0.13.4`，`default-features=false`，`json/stream/`**`rustls`**`/query` | 同版本 features **相加**：迁移后会**同时编译 native-tls 与 rustls**。阶段一删除 `transport/http.rs` 后 TUI 不再需要自持 reqwest，应一并移除；`multipart`（内容上传）改由 `Client::upload_content`（`client.rs:671`）承担 |
| `sha2` | **0.10** | **0.11** | 大版本不同 → 两个 crate 并存（可编译，冗余）。若 TUI 的 sha2 用途迁移后消失则自然消解，否则需对齐 |
| `windows-sys` | **0.60**（`Win32_System_Threading`） | **0.59**（`Win32_Foundation` + `Win32_System_Threading`） | Windows 上并存两份；建议统一到 0.59 |
| `serde_json` | `1`（默认） | `1` + **`preserve_order`** | feature 相加 → **TUI 的 JSON map 也会变成保序**。属行为变化，迁移后需回归一遍依赖 key 顺序的渲染路径 |
| `tokio` | `rt-multi-thread/sync/time/io-util` | 另需 `macros`/`process` | features 相加，无冲突 |
| `uuid` / `thiserror` / `serde` | 1(v4) / 2 / 1(derive) | 同 | ✅ 一致 |

路径依赖：本机两仓为**同级目录**，故为 `qaqh-client = { path = "../qaqh-backend/crates/qaqh-client" }`
（09-14 评估写的 `../../qaqh-backend/...` 是 Windows `D:\project\` 布局）。
`qaqh-client` 自身经 path 依赖 `qaqh-domain`/`qaqh-ringing`/`qaqh-types`，并用
`version.workspace = true`、`[lints] workspace = true`，**path 依赖跨 workspace 可正常工作**，
但要求后端仓始终与该相对路径共存（离线/单独分发 TUI 的场景会断）。

### 2.4 枚举缺口：精确到 2 个方法（本次复核，比 09-14 更具体）

`QueryRequest`（`endpoint.rs:12-29`）现有 9 个变体：
`SessionList` / `SessionActivity` / `ConfigLoad` / `WorkspaceStatus` / `WorkspaceList` /
`SkillsListTools` / `WorkspaceDiagnose` / `FsList` / `FsRead`。

TUI 的 **9 处 `.service(` 调用**实际用到 **9 个方法**，逐个对齐后：

| 结果 | 方法 |
|---|---|
| ✅ 枚举已覆盖（7 个） | `QueryRequest`：`session.list`、`session.activity`、`config.load`；`ActionRequest`（`endpoint.rs:54+`）：`ConfigSave`、`ConfigSetPermissionLevel`、`ProfileApply`、`WorkspaceSetMode` |
| ❌ 缺（**2 个**） | `session.dashboard`、`todo.status`（两个枚举内均零命中） |

（两个枚举都是封闭的：`QueryRequest` `endpoint.rs:12-29`、`ActionRequest` `:54+`。上表 9 项逐个核对过。）

**关键事实：这 2 个方法服务端早已实现**——`qaqh-runtime/src/service.rs:418`（dashboard）、
`:551`（todo.status），鉴权档位也已定义（`service_methods.rs:56`/`:86`，均 `READ_SEEDED`）。

> **因此这不是「请后端加能力」，而是「客户端封闭枚举没跟上服务端」**：后端无需写任何服务端逻辑，
> 只需在 `QueryRequest` 补 2 个变体 + 2 行 `into_parts` 映射 + 2 个测试（约十行）。
> 建议**按补丁提，不要按「阶段二排期」提**——它挡着 2419 行协议镜像与整套自建 HTTP 的删除。

### 2.5 分三刀，而不是评估里的两阶段（**修订**）

关键约束（本次核实）：TUI 的 `HttpClient` **同时**承载传输生命周期与服务面，而缺的只有 2 个方法、
且它们服务端早已实现。既然缺口这么小，**就不该为它造过渡层**：阶段一原样保留现有 `service()`
路径即可（它已在跑、已有测试，不新增任何东西），把服务面整体推到下一刀。

| 阶段 | 内容 | 前置 | 验收 |
|---|---|---|---|
| **一：传输生命周期与流** | `supervisor` / `channel_stream` / `timeline_stream` / `sse_connect` / `open` / `renew` → `Client::connect_async` + `ClientHandlers` + `activate_timeline` | **已就绪**（D-7 解除） | 见下 |
| **1.5：服务面** | 9 处 `.service(` → `Client::{query, action}` | 后端补那 2 个变体（2.4） | `rg "\.service\(" src/` 零命中 |
| **二：类型权威化** | 删 `protocol/`（2419 行），app 层引用点机械替换为 `qaqh-domain`/`qaqh-ringing`；清 `http.rs` 残留 | 1.5 完成 | `rg "crate::protocol"` 零命中 |

**阶段一具体步骤**

1. `Cargo.toml` 加 path 依赖（2.3），**先不删任何文件**，保证可回滚。
2. `Client::connect_async(ClientOptions)` + `ClientHandlers` 替换 `runtime.rs` 的 `supervisor`
   与两条流任务；回调内转投现有 `AppMsg`，**app 层尽量不改**。
3. per-seed timeline 改用 `activate_timeline(seed)`，删掉 `runtime.rs` 的自建任务表。
4. 删 `http.rs` 的传输部分（`open` / `renew` / `health` / `command*` / `session_list` /
   `bootstrap` / `timeline_page` / `sse_connect` / `parse_reset` / `upload_content` /
   `download_content`，**实测 ≈ 226 行**）与整个 `transport/sse.rs`(234)；**保留**客户端核心
   （`ApiError` 分类、`classify`、`send_json`、`apply_discovery` 凭据热更新）+ `service()` +
   辅助函数，**实测 ≈ 280 行**——它们是阶段 1.5 之前服务面赖以工作的部分。
5. 依赖清理（2.3）：`sha2` / `windows-sys` 可随传输部分一并处理；**`reqwest` 必须留到 1.5**
   （保留的 `classify`/`request` 依赖它），故 TLS 后端重复编译要到阶段二才消除。

**验收（修正评估的规模指标）**

- `cargo test` 全绿，且 **T-02/T-04/T-07 的断言语义必须由等价测试继承**——这三处锁的是行为
  契约而非实现；迁移会删掉 `sse.rs`/`runtime.rs` 里的测试，语义须由 qaqh-client 侧测试
  或新的适配测试覆盖，**不可以"测试跟着代码一起删"了事**。
- 人工：daemon 重启自愈 / 租约过期自愈 / `Lagged` 终止帧恢复。
- 规模：**评估原指标（`transport/` + `runtime.rs` → ≤400 行）不成立**——`http.rs` 不可能降到 0。
  修正目标：`transport/`(1178) + `runtime.rs`(949) = 2127 → **约 700**（`runtime.rs` 仅剩 AppMsg
  适配，`transport/` 仅剩 `http.rs` ≈280 + `mod.rs`）。

**风险**：回调式 API 与现有 `app_rx` 排空逻辑的时序；`serde_json` 保序的行为变化（2.3）；
`service()` 与 `Client` 并存期间的**双份凭据来源**（`apply_discovery` 与 client 自身的 discovery
刷新需指向同一份 `daemon.json`）。
**回滚**：保留 `transport/` 目录不删，切分支即可回退。

## 3. 验证命令

```bash
cd ~/Projects/qaqh-tui-app
cargo test 2>&1 | grep -E "^test result"          # 期望 166 passed / 0 failed
cargo clippy --all-targets                         # 期望零 warning
cargo test timeline_model transport::sse runtime::tests   # 本批三个修复的回归锁

# 迁移前置（后端）
cd ~/Projects/qaqh-backend && cargo check -p qaqh-runtime --lib   # 期望零 error（D-7 已解除）
grep -cE "session\.dashboard|todo\.status" crates/qaqh-client/src/endpoint.rs   # 期望 0 → 阶段二仍阻塞
```

## 4. 已知遗留（未做）

| 项 | 说明 |
|---|---|
| T-01 本体 | 迁移未动工，本批只做可行性核实 |
| T-03 | `Lost` 相位无手动重连入口（`ui/status_bar.rs:28-36`）。迁移后触发路径应走 `Client::close()` + 重建（评估 §3.3 已指出） |
| T-05 | `render_transcript.rs:330` 悬空引用 `docs/markdown-plan.md`（已于 `18463b3` 删除） |
| T-06 | `offloaded` 镜像字段无消费方。**依赖后端 `BUG-2026-09-14-03` 落地**：后端工作区已接通 `enable_turn_offload`（此前是死代码，故 offload 从未真正跑过），一旦提交部署，该字段开始出现 → 届时复核 |
| T-08 | 重建窗口型快照的 `has_more` 语义未定，**需先与后端定语义** |
| **后端 2 个枚举变体** | `QueryRequest` 缺 `SessionDashboard`/`TodoStatus`（2.4）。**这是阶段 1.5 与阶段二的唯一前置**，需后端补；不补则 TUI 永远删不掉那 2419 行协议镜像与自建 HTTP |
| 后端登记册标注 | 领回的条目在后端 `docs/buglist/` 里**尚未标注「已移交本仓」**（未执行，因后端工作树有 22 个文件的他人 WIP，避免叠加） |
| `docs/report/...401卡死-report.md:21` | D-5 状态与实际不符（表里写「已修复」，正文/handoff/代码均为未修）。属他人历史报告，未擅自改动 |

## 5. 下一步建议

1. **给后端提那 2 个枚举变体**（2.4）：`QueryRequest` 补 `SessionDashboard`/`TodoStatus`。
   按**补丁**提，不按「阶段二排期」提——服务端已实现，只差客户端枚举没跟上；补上才能删掉
   2419 行协议镜像与自建 HTTP。
2. **T-01 阶段一开工**：前置已全部就绪，且**不依赖第 1 条**（阶段一完全不碰服务面）。
3. T-03 并入迁移做（重连入口的触发路径在迁移后不同，先做会重复劳动）。
4. T-06 与 T-08 都在等后端动作，不占本仓工时。
