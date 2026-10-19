# Handoff：账号登录 UI 落地（后端服务面解封之后）

> 日期：2026-10-19　执行者：AI（接 `handoff-2026-10-19-upstream-adaptation.md` 的 §6.1）
> 状态：**编译与测试全绿**（`cargo check --all-targets` 0 error；`cargo test --bin qaqh-tui`
> **546 passed / 0 failed**，基线 525 + 本轮 21 条）
> 上一棒：`docs/handoff-2026-10-19-upstream-adaptation.md`（其 §6.1「被上游阻塞」本轮解除）

---

## 1. 后端本轮实际交付了什么（先核对，再动手）

| 后端请求单条目 | 落地位置 | 对 TUI 的意义 |
| --- | --- | --- |
| **R1** 账号面出口 | `qaqh-client/src/service.rs`（`SERVICE_METHODS` 18 条 + `is_allowed`）、`Client::service_call(method, params)`、`service_types.rs`（typed DTO） | `siwc.*` 8 个 + `dsaccount.*` 10 个方法**终于能调**，且响应有权威形状 |
| **R3** 写侧收规范名 | `qaqh-config-api` 的 `canonical_wire` + `ConfigPatch::validate`（trim 后判定，`chat`/`messages` 与旧名并存） | 本仓**保留**旧的写词表 + 读侧归一（幂等）；测试口径随之更新，见 §4 |
| **R4** 类型转发 | `qaqh-client/src/lib.rs:56` `pub use qaqh_types::{CredentialSource, ReasoningKind, Wire}` | 删掉 `qaqh-types` 直接依赖，见 §2 |

契约文档：后端 `docs/spec-service-surface-siwc-dsaccount.md`（参数/形状/命名口径已钉死）；
`service_types.rs` 的 DTO 是它的 typed 版本。**两边必须同步改**，不要在 TUI 手抄第三份。

---

## 2. 本轮已改

### 2.1 R4 清理（纯清理，零行为变化）

`Cargo.toml` 删掉 `qaqh-types` path 依赖，四处 `use qaqh_types::…` 改回 `qaqh_client::…`：
`src/app/settings.rs`（词表漂移护栏的测试）、`src/app/timeline_model.rs:14`、
`src/ui/v2/adapter.rs`（测试内）、`src/ui/v2/transcript.rs:24`（`pub use`）。`Cargo.lock` 随之少一行。

### 2.2 账号面（`siwc.*` / `dsaccount.*`）——**主体**

新增三个文件，本仓第一次有「账号」这件事：

| 文件 | 职责 |
| --- | --- |
| `src/app/account.rs`（约 900 行，含 16 条单测） | **纯逻辑**：`AccountKind`、`Flow` 状态机、`next_poll`（轮询/超时判定）、`apply_siwc_flow`/`apply_ds_flow`（终态/superseded）、`siwc_view`/`ds_view`（品牌口径文案）、`AccountBook`（快照 + 模型目录按档隔离） |
| `src/app/browser.rs` | 系统浏览器打开授权页；**只放行 http(s)**，参数逐个交给 `Command`（不拼 shell 串） |
| `src/app/account_ops.rs` | `service_call` 驱动 + `Value → typed DTO` + 结果落地；tick 轮询；设置页摘要同步 |

接线：

- `Overlay::Account(AccountState)`（**全局** overlay，不绑 session）；
- `ActionResult::Account(AccountResult)`；
- 路由 `WorkspaceRoute::Account` → `workspace::draw_account_card`（与设置卡片同款居中浮层，
  背景 agent 视图 + 压暗，未登记鼠标命中区）；
- 按键（卡片内）：`l` 登录 / `c` 取消 / `x` 登出 / `r` 刷新 / `o` 重开授权页 /
  `u` 管理用量 / `↑↓`+`Enter` 选账户（siwc）/ `b` 余额、`v` 校验、`t` 充值（DeepSeek）；
- 入口两条：`/account` 口令，以及设置页新增的 **「账号登录」行**（`FieldId::Account`，
  Enter 打开；BYOK 档明说「无需登录」，不建空面板）。

### 2.3 模型目录接到 model 选择

- `siwc.models` / `dsaccount.models` 取回后存 `AccountBook`（**按档隔离**：套餐档的 slug
  不会出现在账号档的选择器里）；
- 设置页 `model_options` 非空时，`model` 行 `←→` 在目录里循环（当前值不在目录里时按方向取
  首/末项，不跳项）；目录为空（BYOK / 还没取回）时退回自由文本，**不猜候选**；
- 切档（`settings_cycle` 检测到 credential 变化）与切账户（`profile.select`）后**必须重取**。

### 2.4 `qaqh-ui` 围栏保底（原 §6.2）

`src/ui/v2/markdown/code.rs`：info string 第一个 token 命中 `qaqh-ui`（大小写不敏感）时，
不再高亮 DSL 原文，改渲一张「生成式 UI（本端暂不支持渲染）· qaqh-ui v=0.2 format=tsx」
+ 「交互组件已折叠（N 行 DSL）」的卡片。**完整 DSL 渲染仍属项目级，本轮刻意未开工。**

### 2.5 上游 R3 带来的测试口径修正

`src/protocol/mod.rs` 的 `config_contract_exposes_fields_tui_needs` 里旧反例 `"responses "`
现在**合法**（后端先 trim 再判定）——换成 `"openai-chat"` / `""` / `"gpt-5"`，并补一条
「规范名必须可写」的正向断言。`src/app/settings.rs` 新增
`write_table_and_upstream_value_domain_agree`（写词表 ⊂ 后端值域、规范名 ⊂ 后端值域）。

---

## 3. 不能回退的设计决定（接手请先读这节）

1. **超时也要 cancel**（`account::PollAction::Timeout` → 先发 `sign_in.cancel` 再落失败）。
   留一个进行中的流程会把下一次 `sign_in.start` 挡在 `connection_busy` 上（siwc），
   或让旧尝试与新尝试打架（dsaccount）。
2. **关面板 = 放弃登录**：`Esc` 时若流程仍 pending，先取消再关闭。理由同上——
   面板关掉就没有轮询者了，留个孤流程等于给下一次登录埋雷。
3. **superseded 不是错误**：siwc 靠 `profile_not_found`（daemon 把 `SiwcError` 的
   Display 塞进 `action_failed` 的 message），dsaccount 靠 `attempt_id` 比对。两条路径都
   **停止轮询**、不报红、不 cancel 旧 id。
4. **`sharing == false` 时不得出现套餐标识**；`connecting` 期间同样不得出现。
   只有 `connected && sharing` 才写「在用 ChatGPT 套餐（按套餐额度计费）」——那是计费事实陈述。
5. **`storageError` ≠ 未登录**：DeepSeek 本地凭据读不出来时要显示「本地凭据读取失败 · 请重新登录」，
   不是干净的空态。
6. **金额保留服务端原串**（`0E-16` 就是 0），不转 `f64`；充值/赠金**分开**展示。
7. **浏览器只开 http(s)**；daemon 不 spawn、不内嵌 webview，壳层永不接触 token。
8. **写侧 wire 仍用旧拼写**（R3 之后也保持）：归一恒等、无害，且不会在网关侧引入新分支。
9. **`dsaccount` 的两套拼写别统一**：`status.flow` 是 camelCase、`sign_in.status` 是 snake_case；
   DTO 已分开定义，且有测试挡「顺手统一」。
10. **登录类方法不要求会话**：不要把它接到 `attach`/seed 上（引导页第一件事就是登录）。

---

## 4. 验证

```powershell
cargo check --all-targets          # 期望：0 error（只剩 1 条既有 dead_code 警告）
cargo test --bin qaqh-tui          # 期望：546 passed; 0 failed
```

本轮新增 21 条测试（新增文件内 16 条 + 接线 5 条）：

- `account::tests::*`：credential↔kind 映射、品牌口径五态、DeepSeek 空态 vs 凭据损坏、
  轮询间隔/边界/超时、siwc 终态与 deadline 更新、dsaccount attempt 比对与 `cancelled`、
  superseded 判据、模型目录按档隔离、`AccountBook` 不猜、开工即允许首轮询；
- `browser::tests::*`：scheme 白名单、空白/控制字符、拒绝时不 spawn；
- `protocol::tests::config_contract_exposes_fields_tui_needs`（口径更新）；
- `settings::tests::write_table_and_upstream_value_domain_agree`、
  `settings::tests::account_row_speaks_the_snapshot_and_byok_has_no_login`（含模型目录循环）；
- `ui::v2::route::tests::account_overlay_gets_its_own_card_route`；
- `app::overlay_ops::tests::account_navigation_keeps_the_flow_state`；
- `ui::v2::markdown::code::tests::generative_ui_fences_fold_into_a_placeholder` /
  `ordinary_code_blocks_still_highlight`；
- `ui::v2::workspace::tests::account_card_never_fakes_the_plan_badge`。

改过的文件：`Cargo.toml`/`Cargo.lock`、`src/app/{account,account_ops,browser,mod,overlay_ops,
settings,settings_ops,slash,composer_ops,timeline_model}.rs`、`src/protocol/mod.rs`、
`src/terminal/agent/mod.rs`、`src/ui/v2/{adapter,route,transcript,workspace}.rs`、
`src/ui/v2/markdown/code.rs`。

---

## 5. 待办（按阻塞关系）

1. **真机联调（最重要）**：本轮全部结论都是对着 DTO/契约测的，**没有连过真 daemon**。
   最小验证路径：`config.toml` 设 `credential = "chatgpt_plan"` / `"deepseek_account"` →
   `/account` → `l` → 浏览器授权 → 回终端看流程是否 `已完成`、设置页「账号登录」行是否刷新。
   两处要重点看：`siwc.sign_in.status` 的 `flow_id` 不匹配文案（superseded 判据只认
   `profile_not_found`）、DeepSeek 回调端口被占用时的回退。
2. **`ToolProgress.stream` / `bytes_total`**（原 §6.3，仍未做）：`timeline_model.rs` 的
   `TODO(M3)` 仍在丢它们；接上后可做「参数生成中 N KB」与 stdout/stderr 分色。
3. **`qaqh-ui` 完整 DSL 渲染**：本轮只有保底卡片。若要做，落点在 `markdown/code.rs` 之外
   （需要组件目录、表达式求值、动作回传三块），属项目级。
4. **账号面的剩余 UI 能力**（协议已支持、本轮未接）：
   - `siwc.sign_in.start` 的 `profile_id` / `new_profile` / `label` / `reconsent`（本轮一律 `{}`）；
   - `dsaccount.bonus.ack`（赠金「已读」回执）与 `dsaccount.bonuses` 列表（本轮只画余额里
     的赠金汇总）；
   - `siwc.models` 的目录目前只喂设置页 `model` 行，子代理模型（`subagent.model`）未接。
5. **取消的时序**：`sign_in.start` 在途（`Flow::Starting`）时按 `c` 不会取消（还没有 id）。
   若真机上发现 daemon 在这段时间里已经建了流程，需要在 `SignInStart` 返回后立刻补一次
   cancel（当前实现是返回后正常进入 Pending）。

---

## 6. 环境备注

- 后端仓工作区仍有**大量他人未提交改动**（`qaqh-client`、`qaqh-config-api`、`qaqh-runtime`…）。
  本轮的 R1/R3/R4 结论就是在这些未提交改动上读出来的；合并后行号会漂。
- 本仓 `docs/audit/`、`docs/report/`、`docs/ui-compare-2026-10.md` 仍是**已删未提交**状态
  （上一棒遗留），与本轮无关，不要顺手恢复。
- `cargo test` 的缓冲区断言注意：CJK 在 `Buffer` 里占两格（第二格是占位空格），
  直接 `content()` 拼接会出现「已 登 录」——比对前先去掉空白（见
  `account_card_never_fakes_the_plan_badge`）。
