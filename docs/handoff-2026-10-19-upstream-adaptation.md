# Handoff：对齐后端 2026-10-14~17 变动（TUI 侧）

> 日期：2026-10-19　执行者：上一棒（AI）　接手范围：`qaqh-tui` 全部源码
> 状态：**编译与测试全绿**（`cargo check --all-targets` 0 error；`cargo test --bin qaqh-tui` 525 passed / 0 failed）
> 后端请求单：[`qaqh-backend/docs/analysis-client-service-surface-gaps-2026-10-19.md`](../../qaqh-backend/docs/analysis-client-service-surface-gaps-2026-10-19.md)

---

## 1. 基线

| 对象 | 位置 |
| --- | --- |
| TUI 最后一次**功能**提交 | `e21d39c`（2026-10-08）/ `560c0f5`（2026-10-10），此后只有 license 整理 |
| 后端 HEAD | `8008bcc`（2026-10-17） |
| 依赖方式 | path 依赖直吃 `qaqh-client` / `qaqh-config-api`（本轮新增 `qaqh-types`，见 §4.6） |

type 层是**自动跟随**的——所以上游改形状会直接编译报错（本轮开头那条 E0004 就是），
但**能力层不会自动跟随**：新增字段/端点到了客户端只是"多一个没人读的字段"。
本轮的活基本都在能力层。

---

## 2. 本轮已改（A：编译阻塞）

### 2.1 `TimelineEvent::UsageSampled` 未覆盖（E0004）

`TimelineEvent` 新增了只有 HUD 用途的旁路事件，`timeline_model::apply` 的穷尽 match 未覆盖。

改法**不是**补 `todo!()`，而是按后端契约（`qaqh-domain/src/timeline.rs:499-508`、`:604-611`）在
**入口早退**：`src/app/timeline_model.rs:655-665`。理由：协议明确它**不要求回合/块已开**
（端点 `message_start` 就可能先报），若落到 `find_turn_mut` 之后，每一帧采样都会把
`dropped_missing_turn` 记成契约异常——那是 B1 的"契约破坏"信号，不能被正常流量污染。
match 内保留 `E::UsageSampled { .. } => unreachable!(...)`（`:910`），与 `TurnOpened` 同款。

### 2.2 22 处测试夹具缺 `reasoning_kind`

`TimelineBlock` 新增 `reasoning_kind: Option<ReasoningKind>`（`qaqh-domain/src/timeline.rs:380`），
`--all-targets` 下 22 个测试字面量缺字段 → 全部补 `reasoning_kind: None`。
涉及：`src/app/timeline_model.rs`、`src/terminal/agent/mod.rs`（仅 `#[cfg(test)]` 区域）。

---

## 3. 本轮已改（B：能力适配 5 项）

| # | 项 | 落点 | 行为 |
| --- | --- | --- | --- |
| B1 | 流中 usage 落 HUD | `src/app/session.rs:787-831 merge_sampled_usage` + `src/app/mod.rs:1211-1218` | 逐字段"有才更新"；缺报不清零；`total` 缺报按分项自洽；缓存标记粘性；有变化才 `force_redraw` |
| B2 | `reasoning_kind` 全链 | `timeline_model.rs:275-280` 字段 + `from_wire`；`transcript.rs:366-371` `Thinking.kind`；`transcript.rs:555-575` 头部标注「· 摘要」；`adapter.rs:222-242` | `Encrypted`/`Redacted` 依 `is_displayable()` **整块丢弃**；`Summary` 必须标注 |
| B3 | Wire 规范名归一 | `settings.rs:42-64 wire_daemon_spelling` + display/cycle | 读侧把 `chat`/`messages` 折回写拼写；**写侧永远旧拼写** |
| B4 | `ultra` 档 | `settings.rs:19-25` | 六值循环，读回不跳档 |
| B5 | `credential` 设置行 | `settings.rs:71`(FieldId) `:121-126`(ROWS) `:355`(credential_now) `:384`(display) `:763`(cycle) | 三档开关；切档时**只**归位 `wire`，不碰 `baseUrl`；账号档下 API Key 行提示「不使用此键」 |

### 3.1 顺带修掉一条**既有** Windows 红灯

`tool_header_shortens_paths_but_body_keeps_them` 在 Windows 上一直红：头部缩短逻辑只读 `HOME`，
而 Windows 通常只设 `USERPROFILE` → `~` 替换永不发生。

- `transcript.rs:1215 home_dir()`：`HOME` → `USERPROFILE`；
- `transcript.rs:1233 shorten_header_paths_with_home(...)`：把逻辑抽成**可注入 home 的纯函数**，
  测试不再改进程环境（`set_var` 在 2024 edition 是 unsafe 且与并行测试冲突）；
- 新增确定性测试 `header_shortening_tilde_is_explicit_and_boundary_safe`（含 `/home/uu` 边界反例）。

---

## 4. 不能回退的设计决定（接手请先读这节）

1. **`UsageSampled` 必须早退在 turn 查找之前**（§2.1）。挪到下面 = 契约计数器被正常流量污染。
2. **写侧 wire 只能是旧拼写**（`openai`/`responses`/`anthropic`/`gemini`）。
   `ConfigPatch::validate` 至今只收旧名（`qaqh-config-api/src/lib.rs:314`），直发 `chat` = 400。
   桌面端为此也有一道桥（`qaqh-desktop-app/src/oauth/provider-state.ts:81-83`）。
   若后端按后端单 R3 放开了规范名，这里**仍可保留**归一（幂等，无害），但不要把写侧改成规范名再等后端追。
3. **切 `credential` 时只归位 `wire`，绝不猜 `baseUrl`**。三家端点形态不同（gemini 走 query key、
   套餐/账号各有官方 base），猜一个填进用户配置是最坏的兜底。
4. **`Encrypted`/`Redacted` 思考必须整块丢弃**，不要画空壳——空壳会让用户以为"模型什么都没想"。
5. **`merge_sampled_usage` 不做整包替换**。采样只报部分字段是常态（Anthropic `message_delta`
   只带 output），替换会把上下文占比打空、把缓存命中率从"有"变成"没上报"。
6. **`qaqh-types` 的直接依赖**（`Cargo.toml` 注释）是为了 `ReasoningKind`/`Wire`：
   `qaqh_client` 的出口没有转发它们（后端单 R4）。**R4 落地后可以删掉这条依赖**并把
   `use qaqh_types::...` 改回 `qaqh_client::...`，属于纯清理。

---

## 5. 验证（复现命令）

```powershell
cargo check --all-targets          # 期望：0 error（只剩 1 条既有 dead_code 警告）
cargo test --bin qaqh-tui           # 期望：525 passed; 0 failed
```

本轮新增 8 条测试：`ultra_effort_survives_the_cycle_table`、
`canonical_wire_spellings_do_not_reset_the_cycle_cursor`、
`every_upstream_wire_spelling_maps_into_the_local_vocabulary`（漂移护栏，直接对
`qaqh_types::Wire::as_str()` 断言）、`credential_cycle_keeps_wire_compatible_and_flags_the_unused_key`、
`usage_sample_updates_hud_and_missing_fields_are_kept`、`summary_reasoning_is_labelled_and_plain_text_is_not`、
`non_displayable_reasoning_is_dropped_and_summary_passes_through`、
`header_shortening_tilde_is_explicit_and_boundary_safe`。

改过的文件：`Cargo.toml`、`Cargo.lock`、`src/app/{export,mod,session,settings,timeline_model}.rs`、
`src/terminal/agent/mod.rs`、`src/ui/v2/{adapter,transcript,workspace}.rs`。

---

## 6. 待办（按阻塞关系排序）

### 6.1 被上游阻塞：套餐 / DeepSeek 账号登录 UI

> **2026-10-19 已解除**：后端 R1/R4 落地（`Client::service_call` + `service_types`），
> TUI 侧账号面板已完成，见 [`handoff-2026-10-19-account-login-ui.md`](handoff-2026-10-19-account-login-ui.md)。

**不是"没 UI"，是"调不到"**：`qaqh_client` 的 `QueryRequest`（`endpoint.rs:36`）/
`ActionRequest`（`:130`）是封闭枚举，`into_parts` 是 `pub(crate)`，
daemon 那 18 个 `siwc.*` / `dsaccount.*` admin 方法在 Rust 客户端库里没有任何出口。
后端请求单已写（见文首链接 R1）。

解封后的实现建议（照搬桌面端已验证的形态）：

- 登录驱动做成**可注入依赖的纯函数**（桌面端：`src/lib/siwc.ts:134 runSignInFlow`、
  `src/dsaccount/flow.ts:51 runDeepSeekSignIn`），两个易错点必须复现：
  **超时也要 cancel**（否则下次登录撞 `connection_busy`）、**被新登录顶掉 → superseded 而非抛错**；
- 用**系统浏览器**打开 `authorizeUrl`（TUI 无内嵌 webview）；
- 状态→文案按 `qaqh-desktop-app/docs/siwc-brand-posture.md:36-45` 的硬规则，
  尤其 `sharing=false` 时**不得**出现套餐标识（那是计费事实陈述）；
- 模型目录（`siwc.models` / `dsaccount.models`）接到 model 选择处；切档后必须重取。

### 6.2 不被阻塞：`qaqh-ui` 围栏保底（约 40–60 行）

后端 prompt 已默认 `qaqh-ui v=0.2` 围栏，本仓零识别 → 用户看到整段 DSL 原文。
保底做法：在 `src/ui/v2/markdown/code.rs` 的 info string 判定处识别 `qaqh-ui`，
渲染一张"生成式 UI（本端暂不支持）"卡片；完整 DSL 渲染属项目级，别顺手开工。

### 6.3 不被阻塞：`ToolProgress.stream` / `bytes_total`（约 40 行）

后端已在发（`engine_tool.rs:959` 等），本仓显式丢弃（`timeline_model.rs:841-847` 的 `TODO(M3)`）。
接上后可做"参数生成中 N KB"与 stdout/stderr 分色。

---

## 7. 环境备注

- 工作区里 `docs/audit/`、`docs/report/`、`docs/ui-compare-2026-10.md` 处于**已删未提交**状态，
  与本轮改动无关，不要顺手恢复或清理。
- `Cargo.lock` 本轮有变更（新增 `qaqh-types` 依赖），属预期。
- 后端仓工作区当前有大量**他人的未提交改动**；后端单里的行号是"当前工作区"坐标，
  合并前可能漂移，各条都附了 `git grep` 复现命令。
