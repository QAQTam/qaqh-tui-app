# TUI 已知缺陷跟踪清单（以代码为准）（2026-09-15）

## 0. 元信息

| 项 | 值 |
|---|---|
| 清单日期 | 2026-09-15 |
| 基准代码 | TUI：`main` @ `56c31e7`（T-01 阶段一迁移已提交）。后端：`qaqh-backend` @ `a72ce0c`（阶段一所需能力已提交；工作树另有**他人** workspace/工具侧重构 WIP，未提交，本清单不涉）。首版核定时为 TUI `59541dd` / 后端 `1c92413` |
| 判定原则 | **以工作树真实代码为准**。每条状态由 `file:line` + 可复现命令核定；历史文档（report / handoff / 已删除的 `streaming-edge-audit.md`）的自述状态仅作线索，不作依据 |
| 核定环境 | Arch Linux 7.2.4 / rustc 1.98.1。**首版**：`cargo test` 166 passed / 0 failed；`cargo clippy --all-targets` 零 warning。**本轮（阶段一迁移后）**：`cargo test` = **142 passed / 0 failed**（条数下降见 §5b 对账，非静默减少）；`cargo clippy --all-targets` 零 warning；后端 `cargo test -p qaqh-client` = **43 lib + 2 集成 passed / 0 failed** |
| 命名约定 | `docs/buglist/以yyyy-mm-dd-标题-buglist.md作为命名` |
| 状态图例 | `OPEN` 待修 ｜ `FIXED` 已核实修复（附回归锁）｜ `ACCEPTED` 知情接受 ｜ `EXTERNAL` 他仓/环境，本仓不可核实 |

## 1. 待办（OPEN）

| ID | 级别 | 项 | 代码事实（核定证据） | 影响 | 建议动作 |
|---|---|---|---|---|---|
| T-01 | P1 | 传输层未迁移 `qaqh-client` | ~~依赖表零 `qaqh-*`~~ → **阶段一已迁移**：`Cargo.toml:19` 已有 path 依赖，`grep -c qaqh Cargo.lock` = **11**；自建轮子符号（`SseDecoder`/`supervisor_action`/`stream_rebuild`/`timeline_manager`/`refresh_credentials`/`build_envelope`）**零命中**；`transport/` + `runtime.rs` 由 **2127 → 832 行** | — | **阶段一闭环**（见 §2）。**阶段 1.5 阻塞已解除**：后端 `QueryRequest` 已补 `SessionDashboard`/`TodoStatus`（后端 @`a72ce0c`），可开工。**阶段二**（删 `protocol/` 2419 行）前置亦已就绪 |
| ~~T-03~~ | ~~P2~~ | ~~无用户可触发的重连入口~~ | 见 §2「T-03 闭环」 | — | **已闭环（2026-09-15）** |
| T-05 | P3 | 悬空文档引用 | `src/app/render_transcript.rs:330` 注释指向 `docs/markdown-plan.md`，该文件已于 `18463b3 clean docs` 删除（`git log --diff-filter=D` 可证） | 注释把读者引向不存在的设计文档 | 改指向现存文档或删除该引用 |
| T-06 | P3 | `offloaded` 镜像字段**无消费方** | TUI：`protocol/timeline.rs:109` 定义 + `timeline_model.rs` 仅测试夹具写 `offloaded: false`，生产代码**零读取**。后端：侧车缺失/损坏/代际不符时保留壳（`timeline_hub.rs:672-675`），壳带 `offloaded=true`、block 文本 ≤512 字符、`tool.output/diff = None`（`qaqh-runtime/src/timeline.rs:881-898`） | 该退化路径下 TUI 会把「预览壳」当完整回合渲染，用户无从得知（与 B1「丢弃必须可见」同一设计原则） | transcript/状态栏标出「已归档，内容为预览」；或至少读 `offloaded` 给出提示。**注意**：该路径本机尚未发生过，根因见 §8（后端 `BUG-2026-09-14-03` —— offload 此前是死代码；后端工作区已接通，**一旦提交部署即转为可触发**） |

| T-08 | P2 | 重建窗口型快照的 `has_more` 语义未定 | TUI 诚实消费 `has_more`（`timeline_model.rs:302`/`:531`/`:561`，翻页门控 `transcript_ops.rs:191`）；但后端从 messages 重建后窗口被裁剪，`has_more` 是否表达「窗口之前仍有历史」**未定义** | 用户可能翻不到窗口之前的回合（后端提示只能读 `messages.jsonl`） | **需先与后端定语义**，再决定 TUI 是否给「已被裁剪」提示。**领自后端登记册 `BUG-2026-09-12-04` 遗留②** |

> **闭环记录（2026-09-15）**：T-02（流首 BOM）、T-04（D-3 回归锁）、T-07（`TurnOpened` 原地 reopen）已闭环，见 §2。
> **同日追加（T-01 阶段一）**：T-01 阶段一、T-03 同日闭环，并连带改判 D-1/D-2（见 §2）。
> 现余待办：**T-01 阶段 1.5 / 阶段二**（前置均已就绪）、T-05、T-06、T-08。

## 2. 已核实修复（FIXED）

> **阶段一迁移后的行号时效**：T-01 阶段一删掉了 `src/runtime.rs` 的旧实现与整个
> `src/transport/sse.rs`，故下表 D-1/D-2/D-3/D-4、T-02 各行的**旧行号已失效**。
> 这些缺陷本身**仍然成立**（其语义已上移至 `qaqh-client`，落点见每行「迁移后落点」
> 一栏与 §2 末的四行改判）；但按本清单「以工作树真实代码为准」的原则，旧行号
> **不得**被当作现行证据引用——要引用请用右列的新落点。
> 未被迁移触及的行（B1、T-07）行号仍然有效。

| 报告 ID | 项 | 代码落点（实测） | 回归锁（实测通过） |
|---|---|---|---|
| D-1 | 401 三态分类 | ~~`http.rs:63` `is_fatal`/`:68` `is_credential`；`runtime.rs:176` `supervisor_action`~~ → **已改判**，见下方「D-1 改判」行 | 同上 |
| D-2 | 凭据热更新 | ~~`http.rs:110` `apply_discovery`（仍有效）；`runtime.rs:193` `refresh_credentials`（已删）~~ → **已改判**，见下方「D-2 改判」行 | 同上 |
| D-3 | 流感知重协商 | ~~`runtime.rs:339` `stream_rebuild`~~ → **迁移后落点**：`qaqh-client/src/sse.rs:201` `reconcile_epoch`（频道流 epoch 归零）、`src/timeline.rs:36` `observe_epoch`（timeline 重定基） | `qaqh-client` `sse::tests::{epoch_change_resets_cursor, same_epoch_keeps_cursor, first_connect_records_epoch_without_side_effects}`、`timeline::tests::{epoch_change_requires_rebaseline, same_epoch_does_not_rebaseline, first_connect_does_not_rebaseline}` |
| D-4 | 终止帧归一 | ~~`runtime.rs:316` `STREAM_TERMINATED` 等~~ → **迁移后落点**：`qaqh-client/src/sse.rs`（`ringing.stream_terminated` → `ClientError::Transport`）、`src/timeline.rs` 同款 | `qaqh-client` `sse::tests::lagged_termination_frame_normalizes_to_transport`、`timeline::tests::lagged_termination_frame_normalizes_to_transport`（另有新增：终止帧**不推进 cursor**） |
| B1 | 丢弃可观测化 | （未受迁移影响，行号有效）`timeline_model.rs:307`/`:310` 计数、`:320` `dropped_summary`、`:378`/`:515` 计数点；`status_bar.rs:80` 非零时展示 | `timeline_model` 内 3 处断言（`:1427`、`:1449`、`:1484` 断言重放不计入） |
| T-02 | SSE 流首 BOM 剥离（原 D-5 / BUG-2026-09-13-17） | 本仓 `sse.rs` 已删 → **迁移后落点**：`qaqh-client/src/sse_decoder.rs`（`BOM` + 一次性 `bom_checked` 剥离，含跨 chunk 未到齐时等待）。本仓那 4 条断言已补进对端：新增**流中段 U+FEFF 不得被剥离**；并给 BOM 测试补上其测试名早已承诺、原先却没写的 `cursor_from_sse_id == Some(7)` | 本仓 4 条已删 → 对端 `qaqh-client` `sse_decoder::tests::leading_bom_is_stripped_and_first_frame_cursor_survives`（含上述补强）、`leading_bom_does_not_break_frame_payload`、`bom_split_across_chunks_is_handled`、`bom_is_stripped_only_at_stream_start`（**变异验证**：把一次性守卫换成每帧都剥离 → 恰好 1 红） |

| T-07 | `TurnOpened` 镜像后端「原地 reopen」（原 `BUG-2026-09-12-04` 遗留①） | 应用层 `Turn` 补回被丢弃的 `sealed` 字段（`timeline_model.rs:249` 定义、`:277` `from_wire`、`:543` `TurnSealed` 置位）；reopen 分支 `:391`：已存在且 `sealed` → 原地重置（`user_text`/`state=Running`/`failure=None`/`sealed=false`/`rounds.clear()`）并 `bump()`；运行中的重复仍 no-op | `app::timeline_model::tests::sealed_turn_is_reopened_in_place`、`running_turn_duplicate_opened_keeps_content`（**双向变异验证**见 §6）；既有 `duplicate_and_replayed_entries_are_idempotent` 保持通过 |

| **T-01 阶段一** | 传输层迁移 `qaqh-client`（连接生命周期 / 三频道流 / per-seed timeline） | TUI @`56c31e7`：`Cargo.toml:19` path 依赖；`src/runtime.rs` 949→504（`supervisor`/`channel_stream`/`timeline_stream`/`timeline_manager`/`stream_rebuild` 全删）；`src/transport/sse.rs`(318)、`src/transport/discovery.rs`(228) **删除**；`src/transport/http.rs` 624→318（仅剩服务面 `service()`）；新增 `src/protocol/bridge.rs`（唯一的类型转换缝） | 见 §5b 对账 |
| **T-03** | 无用户可触发的重连入口 | `src/app/keymap.rs:51` `Ctrl+R` → `GlobalKey::Reconnect`（`:37`）；`src/app/mod.rs` `request_reconnect`（仅 `Lost` 相位生效）；`src/ui/status_bar.rs:31` 渲染 `· Ctrl+R 重连`；判据 `src/runtime.rs:39` `STALL_AFTER = 15s`（`:362` 检测、`:219` `rebuild()`、`:232` `rebuild_inner`） | `keymap::tests`（既有 8 个保持通过）；真机未验（§5b 遗留） |
| **D-1 改判** | 401 三态分类的**裁决权**上移 | `src/transport/http.rs:31` `ApiError` 与 `classify` 保留（`:41` `LeaseRequired`、`:44` `UnsupportedVersion`）；`is_fatal`/`is_credential` **已删除**（原唯一调用方 `supervisor_action` 随 runtime 重写消失） | `transport::http::tests` 7 个保留（断言改为只查分类） |
| **D-2 改判** | 凭据热更新的**实现**上移至 `qaqh-client` | 后端 @`a72ce0c` `crates/qaqh-client/src/session.rs:108` `refresh_discovery()`、`:92` `credentials()`；`:44`/`:78` `local_discovery` **默认关闭**、由 `connect_async` 在本地发现模式开启（安全默认值：否则指向 mock/远端直连的会话会被本机 `daemon.json` 改道） | `session::tests::{refresh_discovery_is_off_by_default, refresh_discovery_adopts_a_changed_record, constructor_normalizes_trailing_slash}`（变异验证：默认值改回 `true` → 恰好 1 红） |

| **T-09** | `TimelineToolState` 少两个终态（镜像漂移） | 后端 6 变体、本仓镜像 4 个，缺 `Cancelled`/`Backgrounded`（`qaqh-domain/src/timeline.rs:35`，后端注释明写二者是「终态但非失败」）。带这两个状态的 `ToolUpdated` 在镜像侧**反序列化失败 → 整条时间线条目被丢弃**，工具卡永远停在进行中。**已修**（TUI `a278ab8`）：换权威类型 + 补渲染分支 | 渲染分支由编译期强制（非穷尽 match）；无独立回归锁 |
| **T-10** | `BlockCheckpoint` 的 `arg`/`text` 语义弄反（镜像漂移） | 镜像把 `text` 声明为**必填**且不知道 `arg`；权威语义（`qaqh-runtime/src/timeline.rs:371` `checkpoint_block`）是 `arg` = 按**已交付事件**算出的余量（正常路径），`text` = 整流覆盖（正常路径为空且缺席）。后果：正常路径下每个检查点反序列化失败被丢弃；且旧 reducer 的 `block.text = text.clone()` 一旦只把 `text` 放宽为可缺省，就会**每个检查点清空已流出的正文**（本仓最忌讳的「真吞字」）。**已修**（TUI `a278ab8`） | `timeline_model::tests::incremental_checkpoint_appends_and_never_wipes`（**变异验证**：改回无条件覆盖 → 恰好该测试红） |
| **T-11** | `ConfigPatch` 缺 `permission_level`、`ConfigDto` 缺 `mcp`/`lsp`（镜像漂移） | 后端 `qaqh-config-api/src/lib.rs:232` 已加 `permission_level: Option<u64>`（BUG-2026-09-13-15），TUI 镜像没有，且其注释（`protocol/config.rs:116`）仍写着「刻意不含」——**文档断言与后端现状相反**；`ConfigDto` 另缺 `mcp`/`lsp` 两段 | **待修**（阶段二 config 切片） |

**回归测试数量核对**：报告称 14 个（http 7 + runtime 7）。实测 `transport::http::tests::*` 7 个、`runtime::tests::*` **12** 个（原 7 + T-04 新增 5）——报告所载的 14 个**数字一致**，新增的 5 个是本清单补的。（阶段一后 `runtime::tests` 整体删除，见 §5b 对账。）

## 3. 知情接受（ACCEPTED）

| ID | 项 | 代码事实 | 维持理由 |
|---|---|---|---|
| B2 | 重复 `BlockOpened` 整块替换 | `timeline_model.rs:399` `Some(idx) => round.blocks[idx] = wire`（覆盖语义，仍会清空已累计文本） | daemon `DuplicateBlock` 拒绝 + 整流重摆不重放 → 契约上不可达；若未来引入"流内重开块"必须先改此处 |
| C1 | 非法 UTF-8 行整行跳过 | ~~`sse.rs:53-55`~~ → **迁移后落点**：`qaqh-client/src/sse_decoder.rs`（绝不 lossy）。本仓断言已补进对端：「解析器**继续**产出后续帧」那半句原先没写 | 设计行为，保护中英文/emoji 完整性 |
| C2 | 单行无界增长 | ~~`sse.rs:29`~~ → **迁移后落点**：`qaqh-client/src/sse_decoder.rs`（同款「只搬移已消费前缀」游标设计，**仍无单行熔断**）。迁移**未**改变该性质，故仍为知情接受 | daemon 可信；防御性上限留待未来 |
| D1 | `is_gerund_word` 对 4 字母 -ing 词误报 | `render_transcript.rs:49` | 仅影响首行是否提升为标题，无内容丢失 |
| D2 | text/reasoning 原样渲染 `\r` | `apply_bash_progress` 只覆盖 bash 进度（`timeline_model.rs:82`/`:161`/`:192`），text/reasoning 无净化 | LLM 正文罕见输出 `\r` |
| D3 | sealed 瞬间纯文本→markdown 切换 | 渲染语义切换 | 非丢字，`6a7356b` 已根除真吞字；注意与真吞字区分 |
| D4 | 单块 markdown >500 行截断 | `render_transcript.rs:331-333` | 设计，防 100M 爆存（其注释引用悬空见 T-05） |

> **C3（流首 BOM）原列 ACCEPTED**，判据是"服务端不发送、现实中不可达"。该判据已被 `BUG-2026-09-13-17` 反证 → 升级为 OPEN（T-02）→ 已于 2026-09-15 修复（见 §2）。**教训**：以"上游不会那样发"为由接受的边界，会被现实证伪。

## 4. 跨仓核实（后端可达后）

原「本仓不可核实」一栏全部改为源码核实。后端 `qaqh-backend` @ `1c92413` 与在跑 daemon 的 `build_id` 一致，故下列结论同时具备源码与实机效力。

| 原 ID | 原判 | 核实结论（证据） |
|---|---|---|
| D-7 / O-3 | 后端 HEAD 编译失败，`hub.rs:308` 11 error | **已不成立**：`cargo check -p qaqh-runtime --lib` → exit 0 / Finished。旧报告所载的 11 error 已修复，联调阻塞解除 |
| A1 | 文档自述已实施 | **代码证实**：`gate.rs:157` `emit_final_block_checkpoint` + 4 处调用（`:309`/`:565`/`:678`/`:826`） |
| BUG-2026-09-12-11 | 后端侧编号，未核 | **代码证实且契约逐字对齐**：daemon 在 `Lagged` 时发 `ringing.stream_terminated`（`axum_server/axum_impl/sse.rs:456` 频道流 / `:566` timeline 流），事件名与 TUI `runtime.rs:316` 常量**逐字一致**；载荷 `{code:"lagged", channel\|seed, skipped, message}` 与 TUI `stream_terminated_code()` 所解析的 `code` 字段对齐 |
| O-4 | daemon 日志 0 字节 | **本机不成立**：`~/.config/qaqh/qaqh-daemon.log` = 218478 字节且在持续写入（02:09 仍在更新）。Windows 事故时的 0 字节日志属环境问题，非代码缺陷，本清单撤销该条 |
| O-6 | 线上部署 `e61efe07` 未含分片修复 | 仍属部署环境项，但本机在跑 daemon 已是 `1c92413`（远新于 `e61efe07`），仅在旧部署上成立 |
| — | 协议镜像字段正确性 | **逐字段对齐**：`progress_truncated`（`qaqh-domain/src/timeline.rs:121`）、`offloaded`（`:167`）、`ToolProgress.truncated`（`:229`）的 serde 属性与 TUI 侧**逐字相同**（含 `skip_serializing_if = "std::ops::Not::not"`），TUI 的镜像未走样 |

### 4b. 一条被证伪的假设（留档，避免重复上报）

读后端 offload 代码时，本清单作者曾假设「sealed 回合被壳化 → TUI 整流重摆后全文消失」，并准备登记为缺陷。**E1 实测证伪**：

- **代码**：分页响应返回前调用 `rehydrate_timeline_page`（`timeline_api.rs:120-122`）；侧车可用时**恢复全文并把 `offloaded` 重置为 `false`**（`timeline_hub.rs:688-689`）——客户端拿到的是全文而非壳。
- **实机**：`GET /ringing/v1/sessions/e993c719/timeline?limit=100` 返回 10 个 sealed 回合、**全文完整**（单块最大 44644 字符；十回合的 56/65/33/39/20/1/17/27/26/11 个工具块中，output 与 diff 同时为空的**均为 0**），`offloaded` 字段全部缺席；数据根下也不存在 `ringing-offload/` 侧车目录。

结论：**正常路径不存在内容丢失**，不登记为缺陷。残余风险（侧车缺失/损坏/代际不符时才保留壳）另立 **T-06**，并按实机证据标为潜在项。

## 5. 本轮核定纠正的失真

1. **D-5 状态错误（已纠正并已修复）**：`docs/report/2026-09-14-TUI连接生命周期401卡死-report.md:21` 将其标为"已修复"，但**同报告 §7 写"本轮未修"**、handoff 的"已知遗留"表也列未修、代码实测同样未修——四处中三处与表格矛盾。本清单先改判为 `OPEN`（T-02），随后于同日**真正修复**（见 §2）。
   → 仍**待办**：把该报告第 21 行的状态统一为"已修复（2026-09-15，见 buglist T-02）"（属他人报告的历史记录，本次未擅自改动）。
2. **`streaming-edge-audit.md` 的 C3 判据失效**：原判"现实中不可达"被 `BUG-2026-09-13-17` 反证 → 已升级为 T-02 并修复。
3. **该审计文档本身已不存在**：`18463b3 clean docs` 删除，仅存于 git 历史（`git show 18463b3^:docs/streaming-edge-audit.md`）。本清单 §3 已把其中仍有效的 ACCEPTED 项接管过来，避免随文件消失。

## 5b. T-01 阶段一：测试对账、知情接受与失真纠正

### 5b.1 测试条数对账（**不得静默减少**）

166 → 142，逐项可复算：

| 去向 | 条数 | 说明 |
|---|---|---|
| 随 `src/transport/sse.rs` 删除 | −14 | 手写解码器整体上移至 `qaqh-client/src/sse_decoder.rs` |
| 随 `src/runtime.rs` 旧实现删除 | −12 | 5 个 `supervisor_action`（D-1）、2 个终止帧、5 个 `stream_rebuild`（D-3/T-04） |
| 随 `src/transport/discovery.rs` 删除 | −1 | `base_url_strips_legacy_path` → 对端 `discovery::tests` |
| 随 `RingingCommandEnvelope` 删除 | −1 | `command_envelope_validation`；本仓不再组装命令信封（由 client 做） |
| 新增 `src/protocol/bridge.rs` | +4 | 桥的形状保真回归 |
| **合计** | **142** | |

**对端补齐**：`qaqh-client` 由 31 → **43 lib 测试**（+2 集成），其中「安全默认值」与「流中段 BOM」两处做了**变异验证**（改坏实现 → 恰好 1 红）。

### 5b.2 知情接受（新增）

| ID | 项 | 事实 | 维持理由 |
|---|---|---|---|
| E1 | `perf_sse_throughput` 回归锁丢失 | 原测试断言 10k 帧 ≤500ms（防旧 `split_off` 的 O(n²) 搬移回归）。`qaqh-client/src/sse_decoder.rs` 是同款「游标 + consumed 前缀」设计（无搬移），但**该性能锁本身消失** | 设计等价、风险低；若对端将来改成搬移式实现，本仓不会红 |
| E2 | 首屏 timeline 窗口 60 → 30 | `activate_timeline` 用 daemon 默认页（30）；`request_rebaseline`/`load_older` 仍用本仓 `TIMELINE_PAGE_LIMIT = 60` | 与 winui 对齐（两端同源），且 `has_more` 翻页路径不变；**但首屏可见回合数变少是用户可见变化** |
| E3 | timeline 激活对 401 的重试有上限 | 旧实现在 attach 未落地时**无界**重试（400ms 间隔）；现为 `ATTACH_RETRY_ATTEMPTS = 75`（≈30s）后放弃并报 `TimelineLost` | 无界重试会泄漏任务；30s 覆盖真实的 attach 竞态窗口 |

### 5b.3 本轮纠正的三处**上游 handoff 失真**

`docs/handoff/2026-09-15-TUI镜像层修复与qaqh-client迁移阶段一-handoff.md` §2.1/§2.2 的三条判断经源码核实**不成立**，方案据此调整（这是「以代码为准」原则的又一次命中）：

| 原述 | 实测 | 后果 |
|---|---|---|
| `refresh_credentials()` →「内建（`session.rs` 失败计数 → 重新 open）」 | `read_discovery()` 在 `qaqh-client` 里**只在 connect 时调用一次**；`ClientInner.base_url`/`token` 是构造期常量，renew 失败后的 `open()` 用的是**同一对陈旧值** | 照原样迁移会**回归** BUG-2026-09-14-01 的修复（daemon 重启后永不恢复）→ 已由后端 `@a72ce0c` 的凭据热更新补上 |
| 401 三态 `classify` →「`error.rs` `ClientError` 分类」 | `ClientError` 是平铺枚举，**无** `Unauthorized`/`LeaseRequired`/`is_fatal`/`is_credential`，全 crate 无 401 body 解析 | 不是等价映射而是降级 → 分类保留在本仓（服务面需要），裁决权上移 |
| `SseDecoder` →「`sse_decoder.rs`（已含 BOM 剥离）」 | 实现等价，但**断言不全**：流中段 BOM 边界、`cursor_from_sse_id`、终止帧不推进 cursor 均**零覆盖** | 直接删 =「测试跟着代码一起删」→ 已逐条补进对端 |
| （另）不存在的「单活跃 timeline 够用」 | `Client::activate_timeline` 是单槽，激活 B 会静默停掉 A。winui 可用（单 `chat_view`），**TUI 不可用**：子代理发现依赖父会话的流存活，多标签各自 spawn | 照 handoff §2.5 直译会引入行为回归 → 后端改为 `HashMap<seed, TimelineHandle>` |

### 5b.4 后端缺口登记（T-04 的对偶）

D-3/T-04 锁的是「epoch 变化必须归零 cursor」。核实发现 **`TimelineStream` 早已这么做，而 `ChannelStream` 从不比较 epoch**（无 `last_epoch` 字段）——即该不变式在后端只覆盖了一半。已在 `qaqh-client` 补 `reconcile_epoch`（`src/sse.rs:201`）并加锁。**属后端缺陷，已在本仓 §2 以「迁移后落点」记名；后端登记册的正式条目见 `qaqh-backend/docs/buglist/`。**

### 5b.5 遗留（**未验证**，本清单不声称已通过）

- **真机端到端一条未跑**：daemon 重启自愈、租约过期自愈、`Lagged` 终止帧恢复、多标签+子代理并发、`Ctrl+R` 重连。
- 依赖真机的两个集成测试（含本轮新增的**多 seed 并行**那条）仍是 `#[ignore]`，**只做到编译通过**；`lease_renegotiation.rs` 需要 `cargo build -p qaqh-daemon`。
- **【2026-09-15 更正】** 此处原写「后端工作树正被他人的重构占用，编 daemon 会连带编进半成品」——**该判断不成立**：后端工作树实际可编译（`cargo check -p qaqh-daemon` 通过，`cargo build -p qaqh-daemon` 17.8s 成功）。**真实的阻塞是：daemon 在隔离 data root 下启动后不发布 `daemon.json`**（实测 60s 无产出，日志停在 `qaqh_runtime::registry: exec shell bootstrap: bash`；换用真实 `HOME` 复现同样现象，故非隔离环境所致）。该卡点位于他人正在改动的 exec/registry 区域，**本清单未做归因**（未从 HEAD 干净构建对照）。
- 跑该测试时另修掉两个**测试自身的**缺陷（见后端 `ed6ebb3`）：`find_daemon_binary` 硬编码 `.exe` 导致**非 Windows 上必然 panic**（即所谓「端到端覆盖」在本机从来跑不起来）；以及本文件两个测试争抢进程级 `QAQH_DATA_DIR`。

## 6. 核验命令（可复现）

```bash
cd ~/Projects/qaqh-tui-app

# ── 全局基准（本轮：阶段一迁移后） ──
git log -1 --format='%h %ad' --date=short                      # 56c31e7 2026-09-15
cargo test 2>&1 | grep -E "^test result"                        # 142 passed; 0 failed
cargo clippy --all-targets                                      # 零 warning
cd ~/Projects/qaqh-backend && cargo test -p qaqh-client 2>&1 | grep -E "^test result"
#                                                               # 43 lib + 2 集成 passed；2 ignored（需 daemon 二进制）
# 注：后端工作树另有他人 WIP；上面只跑本仓相关 crate，勿整仓 cargo check

# ── T-01 阶段一已迁移 ──
grep -n "qaqh-client" Cargo.toml                                # :19 path 依赖
grep -c qaqh Cargo.lock                                         # 11（原为 1）
grep -rn "SseDecoder\|supervisor_action\|stream_rebuild\|timeline_manager\|refresh_credentials\|build_envelope" src/ | wc -l
#                                                               # 0（自建轮子已全部消失）
wc -l src/transport/*.rs src/runtime.rs | tail -1               # 832（原 2127）

# ── 阶段 1.5 前置已解除 ──
grep -cE "session\.dashboard|todo\.status" ~/Projects/qaqh-backend/crates/qaqh-client/src/endpoint.rs
#   # 7 行命中（两个变体定义 + 两处 into_parts 映射 + 测试；原为 0 → 阶段 1.5 可开工）

# ── T-03 重连入口 ──
grep -n "Reconnect" src/app/keymap.rs                            # :37 枚举、:51 Ctrl+R
grep -n "Ctrl+R" src/ui/status_bar.rs                            # :31 Lost 相位提示

# T-02 BOM（已修；**本仓实现已删**，以下命令跑的是对端）
cd ~/Projects/qaqh-backend && grep -n "const BOM\|bom_checked" crates/qaqh-client/src/sse_decoder.rs
cargo test -p qaqh-client sse_decoder                                # 12 passed
#   变异验证（2026-09-15 实测，本仓改的是对端）：把 `if !self.bom_checked` 的一次性
#   守卫换成每帧都尝试剥离 → bom_is_stripped_only_at_stream_start 恰好 1 红。
#   （本仓原有的「把剥离改为空操作 → 3 红」结论随 sse.rs 删除而失效。）

# T-07 TurnOpened 原地 reopen（已修）
grep -n "Some(idx) if self.turns\[idx\].sealed" src/app/timeline_model.rs    # :391
cargo test timeline_model
#   双向变异验证（2026-09-15 实测）：
#   ① reopen 永不命中（guard 加 `&& false`）→ sealed_turn_is_reopened_in_place FAILED
#   ② 所有重复都 reopen（guard 去掉 sealed）→ running_turn_duplicate_opened_keeps_content
#      与既有 duplicate_and_replayed_entries_are_idempotent 同时 FAILED
#   说明两个方向都被锁住。另：初版曾写过一个「reopen 后 seq=0 被接受」的测试，
#   变异①下**仍然通过**——因 B2（重复 BlockOpened 整块替换）本就会重置
#   last_fragment，该测试恒真，已删除。

# T-03（已闭环：Lost 相位现有按键入口）
grep -n "Ctrl+R" src/ui/status_bar.rs                            # :31 按键提示

# T-05 悬空引用
grep -rn "docs/markdown-plan.md" src/                           # render_transcript.rs:330
git log --oneline --diff-filter=D -- docs/markdown-plan.md      # 18463b3

# T-06 offloaded 无消费方
grep -rn "offloaded" src/ | grep -v "offloaded: false" | grep -v protocol/timeline.rs
#   期望：仅 timeline_model.rs 的断言，无生产读取

# §2 回归锁（注意：runtime::tests / transport::sse::tests 已随阶段一删除）
cargo test transport::http::tests                                # 7 passed（D-1 分类）
cd ~/Projects/qaqh-backend && cargo test -p qaqh-client          # 43 lib + 2 集成（语义已上移至对端）

# T-04 / D-3 变异验证（**本仓的 stream_rebuild 已删**，现验对端的对应实现）
#   对端 `qaqh-client/src/sse.rs` 的 reconcile_epoch / `src/timeline.rs` 的 observe_epoch：
#   把 epoch 比较改成恒等（不再归零/重定基）→ epoch_change_resets_cursor 与
#   epoch_change_requires_rebaseline 变红。
#   注：本仓原有的「删 generation 分支 → 2 红」结论随 runtime.rs 重写而失效。
cd ~/Projects/qaqh-backend && cargo test -p qaqh-client sse::tests timeline::tests
```

> 格式化请定向执行（`rustfmt src/runtime.rs`）：裸跑 `cargo fmt` 会连带改动他人 WIP 与既有文件，本仓惯例是 fmt 不纳入他人 WIP（见 `eae0f79`）。

## 7. 提交状态（与本清单的关系）

- `59541dd`：4 个既有 WIP + T-04 的 `runtime.rs` + 本清单，合成一个提交（按所有者要求"一个大 commit"）。
- `8663f82`：T-07（TurnOpened 原地 reopen）+ T-02（流首 BOM）。
- **`56c31e7`（本轮）**：T-01 阶段一迁移（22 文件，+2125/−2159）。工作树此后**干净**。
- 配套后端提交 **`a72ce0c`**（`crates/qaqh-client/`，8 文件）：阶段一所需能力。该提交**只**按路径包含 `crates/qaqh-client/`；后端工作树里另有**他人** workspace/工具侧重构 WIP（52 项），未提交、本清单不涉。

> 注：T-04 期间一次裸跑 `cargo fmt` 曾把 `src/app/session.rs` 的既有格式改动 10 行（纯 fmt，无语义），已还原至 HEAD。**本轮同一现象再次出现**：定向 `rustfmt` 时 `src/app/session.rs` 被一并格式化（该文件在 HEAD 上本就不是 rustfmt-clean，2 处），按同一惯例**已还原、未纳入 `56c31e7`**。副作用：全仓 `rustfmt --check` 会在该文件上报 2 处 diff——这是 HEAD 的既有状态，非本轮引入。

**取证副作用**：§4b 的 E1 实测在本机 daemon 上以 `client_instance_id = "probe-offload-audit"` 开过一个租约（独立于在跑的 TUI，TTL 30s，到期自动失效），仅做只读 GET，未触碰任何会话状态。

## 8. 与后端登记册的关系（重要）

本项目的**权威登记册在后端仓库**：`qaqh-backend/docs/buglist/`（6 份，2026-09-12/13/14）。其登记规则是「一行一个缺陷；详情进 `docs/report/`，本文件只做索引与状态跟踪」，状态口径为 `open` / `fixed（工作区，待提交）` / `fixed @{commit}` / `verified` / `wontfix`。**本清单是 TUI 侧的补充索引，不是第二本账**——同一缺陷以后端 ID 为准，此处只登记后端登记册未覆盖的 TUI 侧条目（T-01/T-03/T-05）。

对应关系（已核对后端登记册原文）：

| 本清单 | 后端登记册 | 关系 |
|---|---|---|
| T-02（流首 BOM） | `BUG-2026-09-13-17`（client + gate 对称修复 `@e909220`） | 同一缺陷：后端已修，TUI 侧长期未同步（漂移实例），2026-09-15 补修 |
| T-06（`offloaded` 无消费方） | `BUG-2026-09-14-03`（`enable_turn_offload` 死代码） | **同源**：后端已登记并说明根因，见下方修正 |
| （已修）非 bash progress 有界 | 登记册 `O-2`，状态 `fixed @59541dd` | 同一改动：后端登记册已引用本仓提交 |
| D-4（终止帧归一） | `BUG-2026-09-12-11`（`fixed @13cb21e`） | 后端侧已修；TUI 侧 D-4 是其消费端对偶，已核实 |
| D-1/D-2/D-3（401 族） | `BUG-2026-09-12-10`（会话身份/lease，`fixed @13cb21e`） | **族相关但根因不同**：后端修的是服务端身份与租约；TUI 的 401 卡死是消费端错误分类自杀（BUG-2026-09-14-01 的 TUI 侧） |

**领回记录（2026-09-15）**：后端登记册里误登记的 TUI 侧条目已全部领回本仓，清单如下——`O-2`（非 bash progress 有界，后端标 `fixed @59541dd`，对应本仓 `59541dd`，已在 §2 记账）；`BUG-2026-09-12-04` 的遗留①②→ 本仓 **T-07**（同日修复，见 §2）/ **T-08**；`BUG-2026-09-13-17` 的 TUI 侧变体 → 本仓 **T-02**（后端未登记该变体，属漏记）。另 `2026-09-12-多会话…401-buglist.md` 记有「未端到端复现 401（前端壳不在本仓）」，那是后端清单自身的覆盖缺口，非 TUI 缺陷，本仓对应项 D-1~D-4 已核实修复。

> 待办（未做）：后端登记册中上述条目尚未标注「已移交本仓」，两边仍是双份记录。需在后端 `docs/buglist/` 相应行补一句指向本清单，**该写操作未执行**（后端工作树另有 22 个文件的他人 WIP 改动，避免叠加）。

**对 T-06 的修正**：本清单初版写「本机实测该路径尚未发生过」。查后端登记册后，原因已明确——不是偶然，而是 `enable_turn_offload` **全仓无调用者**（死代码，`BUG-2026-09-14-03`），offload 生产路径恒不执行。后端工作区已接通该开关（待提交）。**一旦后端提交并部署，offload 将真正开始运行，T-06 随之从「潜在」变为「可触发」**——届时 TUI 若不消费 `offloaded`，侧车缺失/损坏场景会直接呈现给用户。故 T-06 应随后端该提交一并复核，届时可上调优先级。

## 9. 变更记录

| 日期 | 变更 |
|---|---|
| 2026-09-15 | 首版：以 `ba26c8f` 工作树代码为准核定 D-1~D-7 / O-1~O-6 / B1~B2 / C1~C3 / D1~D4 / BUG-2026-09-12-11 / BUG-2026-09-13-17；新开 5 项待办 T-01~T-05；纠正 D-5 状态失真 |
| 2026-09-15 | **T-04 闭环**：D-3 判定抽为 `stream_rebuild` 纯函数（`runtime.rs:339`），两条流共用消除内联漂移；+5 单测（变异验证 10 passed / 2 failed）。测试总数 155 → 160。合成提交 `59541dd` |
| 2026-09-15 | **T-02 闭环**：SSE 流首 BOM 剥离（`sse.rs:13/34/59-69`），按 `qaqh-client` 参照实现逐语义移植（含跨 chunk 等待）；+4 单测（变异验证 11 passed / 3 failed）。测试总数 160 → 164 |
| 2026-09-15 | **T-01 阶段一分析完成**：核实 qaqh-client 实际 API、依赖差异（4 处需处理，含 reqwest TLS 后端不一致与 `serde_json` 保序 feature 相加）、枚举缺口复核；产出 `docs/handoff/2026-09-15-TUI镜像层修复与qaqh-client迁移阶段一-handoff.md` |
| 2026-09-15 | **T-07 闭环**：`Turn` 补回被丢弃的 `sealed` 字段，`TurnOpened` 对已 sealed 回合原地 reopen（镜像后端 `timeline.rs:208-244`）。+2 测试，双向变异验证。测试总数 164 → 166 |
| 2026-09-15 | **领回后端登记册中的 TUI 条目**（§8 领回记录）：`BUG-2026-09-12-04` 遗留①② → 本仓新开 **T-07**（`TurnOpened` 不镜像原地 reopen，P1，已核实当前代码仍成立）与 **T-08**（`has_more` 语义未定，需先定设计）；`O-2`/`BUG-2026-09-13-17` 对应本仓 `59541dd`/T-02，已在 §2 记账 |
| 2026-09-15 | **对接后端权威登记册**（§8）：确认 `qaqh-backend/docs/buglist/` 为本项目登记册（含登记规则与状态口径），建立本清单的条目映射；T-06 与后端 `BUG-2026-09-14-03` 同源并修正其"尚未发生"的根因；本清单定位为 TUI 侧补充索引，不作第二本账 |
| 2026-09-15 | **后端可达后跨仓核实**（§4）：D-7 编译失败已不成立、A1 与 BUG-2026-09-12-11 代码证实、O-4 本机不成立撤销、协议镜像字段逐字对齐；新开 T-06（`offloaded` 无消费方，潜在）；§4b 留档一条被 E1 证伪的假设 |
| 2026-09-15 | **T-01 阶段一闭环**：传输层迁移 `qaqh-client`（TUI `56c31e7` + 后端 `a72ce0c`）。删除自建连接生命周期/SSE 解码/daemon 发现；`transport/` + `runtime.rs` 2127 → **832** 行；自建轮子符号零命中。**T-03 同日闭环**（`Lost` 相位 `Ctrl+R` + 15s 失联判据）。**D-1/D-2 改判**（裁决权上移，见 §2）。测试 166 → 142，逐项对账见 §5b.1；被删断言的语义补进对端（31 → 43）。新增知情接受 E1~E3（§5b.2）。**§2/§3 中旧行号已按「以代码为准」原则标注失效并给出新落点。**真机端到端**未跑**（§5b.5） |
| 2026-09-15 | 纠正三处上游 handoff 失真（§5b.3）：`refresh_credentials` 并非「内建」（照原样迁移会回归 BUG-2026-09-14-01）、401 三态非等价映射、`SseDecoder` 断言不全；另记一条不存在的假设（「单活跃 timeline 够用」对 TUI 不成立） |
| 2026-09-15 | 登记后端缺口（§5b.4）：`ChannelStream` 从不比较 epoch（`TimelineStream` 早已比较）——D-3/T-04 的不变式在后端只覆盖一半；已补 `reconcile_epoch` 并加锁 |
| 2026-09-15 | **阶段二·第一刀（timeline）**：25 处引用改用权威类型，删 `protocol/timeline.rs`。**顺带修掉两处镜像漂移** —— T-09（`TimelineToolState` 少 `Cancelled`/`Backgrounded`）、T-10（`BlockCheckpoint` 的 `arg`/`text` 语义弄反，含一次差点写成的「真吞字」）。新开 **T-11**（config 漂移，待修）。测试 142 → 143，clippy 零 warning。TUI `a278ab8`；后端再导出 `73b24fa` |
