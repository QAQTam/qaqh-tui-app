# TUI 传输层迁移到 `qaqh-client` 评估（2026-09-14）

## 0. 元信息

| 项 | 值 |
|---|---|
| 日期 | 2026-09-14 |
| 范围 | `qaqh-tui-app` 自建传输/协议层 → 复用 `qaqh-backend/crates/qaqh-client` |
| 背景 | `docs/report/2026-09-14-TUI连接生命周期401卡死-report.md` D-1~D-6：TUI 未复用 `qaqh-client`，平行实现导致缺陷漂移 |
| 结论 | **建议迁移，但不建议全量替换**。传输骨架（连接生命周期 + SSE + 重连自愈）应换为 `qaqh-client`；协议类型层因 `QueryRequest`/`ActionRequest` 封闭枚举缺 `session.dashboard`/`todo.status` 等方法，需先扩后端 API 再迁移。分两阶段：阶段一（低风险，解 P0/P1）先迁移连接生命周期；阶段二（需后端配合）迁移服务面与协议类型。 |

## 1. 现状事实（E2）

| 维度 | TUI 自建 | `qaqh-client` |
|---|---|---|
| 依赖声明 | `Cargo.toml:12-38` 无任何 `qaqh-*` | 被 `apps/winui/Cargo.toml:10` 复用 |
| 连接生命周期 | `runtime.rs:202` `supervisor()`（open + 续租循环） | `session.rs` `RingingSession` + `client.rs:138` `connect_async` |
| SSE 流管理 | `runtime.rs:373`/`:553` 三条频道流 + per-seed timeline 流 | `sse.rs` `ChannelStream::run` + `timeline.rs` `TimelineStream` |
| SSE 解码 | `transport/sse.rs` `SseDecoder`（234 行） | `sse_decoder.rs`（284 行，含 BOM 剥离） |
| 协议类型 | `protocol/*` 手工镜像（2413 行） | 直接复用 `qaqh-ringing` / `qaqh-domain` 权威类型 |
| 服务面 | `transport/http.rs` `service(method, params)` 任意字符串 | `QueryRequest` / `ActionRequest` 封闭枚举（`endpoint.rs`） |
| 规模 | transport+runtime+protocol ≈ **4336 行** | client crate ≈ 3091 行 |

TUI 注释自述为镜像：`transport/sse.rs:1`「对照 `qaqh-client/src/sse_decoder.rs` 的行为契约」、`runtime.rs:3`「行为契约（对照 `qaqh-client`）」、`protocol/mod.rs:3`「手工镜像 `qaqh-ringing` 与 `qaqh-domain`」。

## 2. 迁移收益

| # | 收益 | 依据 |
|---|---|---|
| B-1 | 缺陷漂移终止：`qaqh-client` 已修的行为自动获得（BOM 剥离、终止帧归一、重协商广播、401 分类） | 本次 4 个发现中 3 个是「client 已修、TUI 未跟」 |
| B-2 | 类型权威化：删除 2413 行手抄协议，改用 `qaqh-domain`/`qaqh-ringing`，编译期捕获后端协议变更 | `protocol/mod.rs:3` |
| B-3 | 与 winui 同源：两端共享同一传输实现，行为差异收敛到渲染层 | `apps/winui/Cargo.toml:10` |
| B-4 | 删除自建重连/退避/判活逻辑，减少自研面 | `runtime.rs` 现 837 行 |

## 3. 迁移障碍（关键，必须先解决）

### 3.1 服务面封闭枚举缺口（**阻塞阶段二**）

`qaqh-client` 的服务面是封闭枚举（`endpoint.rs:12-29` `QueryRequest`、`:54+` `ActionRequest`），设计意图明确（注释：*"Native shells choose a closed Rust variant; they cannot mistype a method name"*）。

实测覆盖度：

| TUI 需要的方法 | `qaqh-client` 覆盖 |
|---|---|
| `session.list` / `session.activity` / `config.load` / `config.save` / `config.set_permission_level` / `profile.apply` / `workspace.set_mode` / `skills.list_tools` / `fs.list` / `fs.read` | ✅ |
| `session.dashboard`（`app/session_ops.rs:178`） | ❌ |
| `todo.status`（`app/session_ops.rs:242`） | ❌ |
| `session.meta` / `plan.read` / `plan.context_stats` / `stats.token_usage` / `git.*` | ❌（TUI 已定义常量但当前未调用，迁移后若要用同样缺） |

TUI 现用的 `service(method: &str, params)` 是**泛型逃生口**（`http.rs`），迁移后若走 `QueryRequest` 会直接编译失败。

**解决选项**（三选一，需后端决策）：

| 选项 | 做法 | 代价 | 风险 |
|---|---|---|---|
| (a) 扩枚举（推荐） | 在 `QueryRequest` 补 `SessionDashboard{seed}`、`TodoStatus{seed}` 等变体 | 后端小改动，符合既有设计意图 | 低 |
| (b) 加泛型逃生口 | 给 `Client` 加 `raw_query(method, params)` | 破坏「不可拼错方法名」的约束 | 中（弱化纪律） |
| (c) 暂不迁移服务面 | 阶段二只迁传输/流，服务面保留 TUI 自建 | 两套并存，收益打折 | 低但收益小 |

### 3.2 平台与依赖差异

| 项 | TUI | `qaqh-client` | 影响 |
|---|---|---|---|
| reqwest | `0.13`（`Cargo.toml:22`） | 需核对后端锁定版本 | 版本对齐成本 |
| TLS | `native-tls` | 同上 | 低 |
| tokio features | `rt-multi-thread`/`sync`/`time`/`io-util` | 需含 `macros` 等 | 低 |
| windows-sys | `0.60`（`Cargo.toml:41`） | 后端自有版本 | 需统一（否则重复依赖） |

### 3.3 架构差异（需适配，非阻塞）

| 关注点 | TUI 现状 | `qaqh-client` | 适配动作 |
|---|---|---|---|
| 事件投递 | `mpsc::UnboundedSender<RuntimeMsg>`（app 自建消息枚举） | 回调式 `ClientHandlers{on_batch,on_status,on_reset,on_timeline_*}` | 在回调里转投 `AppMsg`；`runtime.rs` 的 `RuntimeMsg` 枚举大部分可删 |
| per-seed timeline | `timeline_manager` 动态增减任务（`runtime.rs:412`） | `TimelineStream` + `activate_timeline` | 用 `activate_timeline(seed)` 替代自建任务表 |
| 运行时 | `tokio::runtime::Builder` 自建（`main.rs:47`） | `qaqh_client::runtime_handle()` 全局运行时 | 二选一，避免双运行时 |
| 重连自愈 | 本次新增 `supervisor_action`/`refresh_credentials` | 内建（`session.rs` 失败计数 → 重新 open；`client.rs:222` 重放 attach） | 迁移后本次修复代码大部分删除（逻辑上移到 client） |
| 手动重连 | 无 | `Client::close()` + 重建 | 可借迁移补 UI 入口（O-5） |

## 4. 分阶段方案

### 阶段一：连接生命周期与流（低风险，可立即做）

**目标**：消除 D-1/D-2/D-3/D-4/D-5 的复发面。

1. `Cargo.toml` 加 `qaqh-client = { path = "../../qaqh-backend/crates/qaqh-client" }`（与 winui 同形）。
2. 用 `Client::connect_async` 替换 `runtime.rs` 的 `supervisor` + `channel_stream` + `timeline_stream` + `transport/http.rs` 的 open/renew/sse_connect。
3. 在 `ClientHandlers` 回调中把事件转投现有 `AppMsg`，尽量不改 app 层。
4. 保留 `protocol/*`（app 层大量引用其类型），仅删除 `transport/http.rs`、`transport/sse.rs`、`runtime.rs` 的传输部分。
5. 服务面继续走 `qaqh-client` 的 `query`/`action`；**若遇枚举缺口，本阶段暂以选项 (c) 兜底**（临时保留一个薄 `service()` 适配器）。

**验收**：
- `cargo test` 全绿（含本次新增 14 个回归测试的等价断言）
- 人工：daemon 重启后 TUI 自动恢复（不需重启进程）
- 人工：流式输出中 kill daemon → TUI 显示重连 → 重启 daemon → 自动恢复
- 代码量：`transport/` + `runtime.rs` 从 1923 行降至 ≤ 400 行（仅保留 AppMsg 适配）

### 阶段二：服务面与协议类型（需后端配合）

**前置**：后端在 `QueryRequest`/`ActionRequest` 补 `session.dashboard`/`todo.status` 等变体（选项 a）。

1. 删除 `protocol/*`，改用 `qaqh-domain`/`qaqh-ringing` 类型。
2. app 层引用点机械替换（`crate::protocol::X` → `qaqh_client::X`）。
3. 删除 `transport/http.rs` 的 `service()` 泛型口。

**验收**：`rg "crate::protocol"` 零命中；`cargo clippy --all-targets` 零 warning。

## 5. 工作量与风险

| 项 | 阶段一 | 阶段二 |
|---|---|---|
| 预估 | 2–3 人日 | 3–5 人日 + 后端 1 人日 |
| 主要风险 | 回调式 API 与现有消息循环的时序（`app_rx` 排空逻辑） | 枚举缺口；协议类型引用点数量 |
| 回滚 | 保留 `transport/` 目录不删，切分支即可回退 | 同上 |
| 不可并行 | 与 `qaqh-backend` `rebase/76` 编译失败（D-7）解耦，可并行 | 需后端枚举先落地 |

## 6. 建议排期

| 优先级 | 项 | 依赖 |
|---|---|---|
| P0 | 先修后端 `rebase/76` 编译失败（report D-7） | — |
| P1 | 后端扩 `QueryRequest`/`ActionRequest` 枚举（选项 a） | P0 |
| P1 | 阶段一迁移 | 后端 crate 可编译 |
| P2 | 阶段二迁移 | 枚举落地 |
| P2 | 迁移后删除 TUI `protocol/`、`transport/` 残留 | 阶段二 |

## 7. 与本次修复的关系

本次已在**不迁移**的前提下修掉 D-1~D-5（`http.rs` + `runtime.rs`，14 个回归测试）。迁移不是这些缺陷的前置条件；但若不迁移，同类漂移会随 `qaqh-client` 的后续修复继续发生（B-1）。因此建议：**本次修复先上线止血，迁移按阶段排期跟进**。

## 附录：核实命令

```bash
# TUI 是否复用 qaqh-client
cd D:/project/qaqh-tui-app && rg -n "qaqh-client" Cargo.toml Cargo.lock   # 期望：无输出

# qaqh-client 服务面覆盖度
cd D:/project/qaqh-backend
rg -n "session.dashboard|todo.status|session.meta|plan.read|git.diff" crates/qaqh-client/src/endpoint.rs
# 期望：无输出 → 确认缺口

# TUI 实际调用的 service 方法
cd D:/project/qaqh-tui-app
rg -n "\.service\(methods::([A-Z_]+)" src

# 规模对照
cd D:/project/qaqh-tui-app && wc -l src/transport/*.rs src/runtime.rs src/protocol/*.rs
cd D:/project/qaqh-backend && wc -l crates/qaqh-client/src/*.rs
```
