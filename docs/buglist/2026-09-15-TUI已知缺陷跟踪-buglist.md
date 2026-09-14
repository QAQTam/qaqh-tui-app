# TUI 已知缺陷跟踪清单（以代码为准）（2026-09-15）

## 0. 元信息

| 项 | 值 |
|---|---|
| 清单日期 | 2026-09-15 |
| 基准代码 | TUI：`main` @ `59541dd`（T-04 闭环已提交；工作树另有 T-02 未提交改动 `src/transport/sse.rs`）。后端：`qaqh-backend` @ `1c92413`，与在跑 daemon 的 `build_id` **一致**（`~/.config/qaqh/daemon.json`），故 §4 的结论同时是源码结论与实机结论 |
| 判定原则 | **以工作树真实代码为准**。每条状态由 `file:line` + 可复现命令核定；历史文档（report / handoff / 已删除的 `streaming-edge-audit.md`）的自述状态仅作线索，不作依据 |
| 核定环境 | Arch Linux 7.2.4 / rustc 1.98.1；`cargo test` = **166 passed / 0 failed**（T-04 +5、T-02 +4、T-07 +2）；`cargo clippy --all-targets` = **零 warning**；后端 `cargo check -p qaqh-runtime --lib` = **零 error** |
| 命名约定 | `docs/buglist/以yyyy-mm-dd-标题-buglist.md作为命名` |
| 状态图例 | `OPEN` 待修 ｜ `FIXED` 已核实修复（附回归锁）｜ `ACCEPTED` 知情接受 ｜ `EXTERNAL` 他仓/环境，本仓不可核实 |

## 1. 待办（OPEN）

| ID | 级别 | 项 | 代码事实（核定证据） | 影响 | 建议动作 |
|---|---|---|---|---|---|
| T-01 | P1 | 传输层未迁移 `qaqh-client` | `Cargo.toml` 依赖表零 `qaqh-*`（`grep -c qaqh Cargo.lock` = 1，且该命中的是 `qaqh-tui` 包名自身，`Cargo.lock:1518`）；平行实现实测 **4546 行**（`transport/` 1178 + `protocol/` 2419 + `runtime.rs` 949），较报告所称 ~3941 行**已继续增长** | 后端每修一个传输缺陷，TUI 需人工追平；漂移已实际发生（BUG-2026-09-13-17 BOM） | **阶段一前置已就绪**（后端编译阻塞 D-7 已解除、API 与依赖差异已核实）→ 见 `docs/handoff/2026-09-15-TUI镜像层修复与qaqh-client迁移阶段一-handoff.md` §2；阶段二仍阻塞于后端 `QueryRequest` 枚举缺口（复核未变） |
| T-03 | P2 | 无用户可触发的重连入口 | `src/ui/status_bar.rs:28-36`：`Lost` 相位仅渲染 `✗ lost` + `truncate_str(err, 30)`；全仓重连语义只有 runtime 内部 `SupervisorAction::Reconnect`（`runtime.rs:162`），**无任何按键到 `client.open()` 的路径** | 一旦进入 `Lost`，用户唯一手段是重启进程 | `Lost` 相位挂按键触发显式重新协商，并在状态栏显示按键提示 |
| T-05 | P3 | 悬空文档引用 | `src/app/render_transcript.rs:330` 注释指向 `docs/markdown-plan.md`，该文件已于 `18463b3 clean docs` 删除（`git log --diff-filter=D` 可证） | 注释把读者引向不存在的设计文档 | 改指向现存文档或删除该引用 |
| T-06 | P3 | `offloaded` 镜像字段**无消费方** | TUI：`protocol/timeline.rs:109` 定义 + `timeline_model.rs` 仅测试夹具写 `offloaded: false`，生产代码**零读取**。后端：侧车缺失/损坏/代际不符时保留壳（`timeline_hub.rs:672-675`），壳带 `offloaded=true`、block 文本 ≤512 字符、`tool.output/diff = None`（`qaqh-runtime/src/timeline.rs:881-898`） | 该退化路径下 TUI 会把「预览壳」当完整回合渲染，用户无从得知（与 B1「丢弃必须可见」同一设计原则） | transcript/状态栏标出「已归档，内容为预览」；或至少读 `offloaded` 给出提示。**注意**：该路径本机尚未发生过，根因见 §8（后端 `BUG-2026-09-14-03` —— offload 此前是死代码；后端工作区已接通，**一旦提交部署即转为可触发**） |

| T-08 | P2 | 重建窗口型快照的 `has_more` 语义未定 | TUI 诚实消费 `has_more`（`timeline_model.rs:302`/`:531`/`:561`，翻页门控 `transcript_ops.rs:191`）；但后端从 messages 重建后窗口被裁剪，`has_more` 是否表达「窗口之前仍有历史」**未定义** | 用户可能翻不到窗口之前的回合（后端提示只能读 `messages.jsonl`） | **需先与后端定语义**，再决定 TUI 是否给「已被裁剪」提示。**领自后端登记册 `BUG-2026-09-12-04` 遗留②** |

> **闭环记录（2026-09-15）**：T-02（流首 BOM）、T-04（D-3 回归锁）、T-07（`TurnOpened` 原地 reopen）已闭环，见 §2。待办为 T-01 / T-03 / T-05 / T-06 / T-08。

## 2. 已核实修复（FIXED）

| 报告 ID | 项 | 代码落点（实测） | 回归锁（实测通过） |
|---|---|---|---|
| D-1 | 401 三态分类、致命判定收敛 | `http.rs:32` `PLAIN_UNAUTHORIZED`、`:178` 按 body 区分三态、`:63` `is_fatal`、`:68` `is_credential`；`runtime.rs:176` `supervisor_action` 纯函数（`:162` 枚举） | `transport::http::tests::plain_lease_expired_is_lease_required_not_unauthorized`、`plain_unauthorized_is_not_fatal`、`unsupported_version_is_the_only_fatal_case`；`runtime::tests::lease_expiry_must_reconnect_not_stop`、`no_non_fatal_error_ever_stops`、`only_protocol_drift_stops_the_lifecycle` |
| D-2 | 凭据热更新 | `http.rs:77-78` `RwLock<String>`、`:110` `apply_discovery`、`:144` `token()`（clone 出锁避免 `!Send`）；`runtime.rs:193` `refresh_credentials` | `transport::http::tests::apply_discovery_hot_swaps_credentials` |
| D-3 | 流感知重协商 | `runtime.rs:339` `stream_rebuild` 纯函数（`:320` 枚举）：epoch 变 → `ResetAndRebuild`（归零）；仅 generation 变 → `Rebuild`（**保留 cursor**）；都不变 → `None`。两条流共用：`:480`（频道流）、`:703`（timeline 流），原先两处内联判定已消除 | `runtime::tests::generation_bump_rebuilds_stream_and_keeps_cursor`、`epoch_change_resets_cursor_and_rebuilds`、`epoch_change_wins_over_generation_bump`、`unrelated_conn_info_change_does_not_rebuild`、`only_unchanged_conn_info_avoids_rebuild`（5 个；**变异验证**见 §6） |
| D-4 | 终止帧归一 | `runtime.rs:316` `STREAM_TERMINATED`、`:355` `stream_terminated_code`、`:369`（频道流）/`:723`（timeline 流）拦截 | `runtime::tests::termination_frame_forces_channel_reconnect`、`stream_terminated_code_parsing_is_total` |
| B1 | 丢弃可观测化 | `timeline_model.rs:307`/`:310` 计数、`:320` `dropped_summary`、`:378`/`:515` 计数点；`status_bar.rs:80` 非零时展示 | `timeline_model` 内 3 处断言（`:1427`、`:1449`、`:1484` 断言重放不计入） |

| T-02 | SSE 流首 BOM 剥离（原 D-5 / BUG-2026-09-13-17） | `sse.rs:13` `BOM` 常量、`:34` `bom_checked` 字段、`:59-69` 在 `next_frame` 开头剥离（含跨 chunk 未到齐时等待、不误判的分支） | `transport::sse::tests::leading_bom_is_stripped_and_first_frame_survives`、`leading_bom_does_not_break_data_only_frame`、`bom_split_across_chunks_is_handled`、`bom_is_stripped_only_at_stream_start`（**变异验证**：把剥离动作改为空操作后恰 3 个变红）；对端参照 `qaqh-client/src/sse_decoder.rs:71-83` |

| T-07 | `TurnOpened` 镜像后端「原地 reopen」（原 `BUG-2026-09-12-04` 遗留①） | 应用层 `Turn` 补回被丢弃的 `sealed` 字段（`timeline_model.rs:249` 定义、`:277` `from_wire`、`:543` `TurnSealed` 置位）；reopen 分支 `:391`：已存在且 `sealed` → 原地重置（`user_text`/`state=Running`/`failure=None`/`sealed=false`/`rounds.clear()`）并 `bump()`；运行中的重复仍 no-op | `app::timeline_model::tests::sealed_turn_is_reopened_in_place`、`running_turn_duplicate_opened_keeps_content`（**双向变异验证**见 §6）；既有 `duplicate_and_replayed_entries_are_idempotent` 保持通过 |

**回归测试数量核对**：报告称 14 个（http 7 + runtime 7）。实测 `transport::http::tests::*` 7 个、`runtime::tests::*` **12** 个（原 7 + T-04 新增 5）——报告所载的 14 个**数字一致**，新增的 5 个是本清单补的。

## 3. 知情接受（ACCEPTED）

| ID | 项 | 代码事实 | 维持理由 |
|---|---|---|---|
| B2 | 重复 `BlockOpened` 整块替换 | `timeline_model.rs:399` `Some(idx) => round.blocks[idx] = wire`（覆盖语义，仍会清空已累计文本） | daemon `DuplicateBlock` 拒绝 + 整流重摆不重放 → 契约上不可达；若未来引入"流内重开块"必须先改此处 |
| C1 | 非法 UTF-8 行整行跳过 | `sse.rs:53-55`（绝不 lossy） | 设计行为，保护中英文/emoji 完整性；有测试 `invalid_utf8_line_is_skipped` |
| C2 | 单行无界增长 | `sse.rs:29` `COMPACT_THRESHOLD = 64K` 只搬移已消费前缀，**无单行熔断** | daemon 可信；防御性上限留待未来 |
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

## 6. 核验命令（可复现）

```bash
cd ~/Projects/qaqh-tui-app

# 全局基准
git log -1 --format='%h %ad' --date=short                      # 59541dd 2026-09-15
cargo test 2>&1 | grep -E "^test result"                        # 164 passed; 0 failed
cargo clippy --all-targets                                      # 零 warning
cd ~/Projects/qaqh-backend && cargo check -p qaqh-runtime --lib  # 零 error（D-7 已解除）

# T-01 未迁移
grep -c qaqh Cargo.lock                                         # 1（qaqh-tui 包名自身）
wc -l src/transport/*.rs src/protocol/*.rs src/runtime.rs | tail -1   # 4462

# T-02 BOM（已修）
grep -n "const BOM\|bom_checked" src/transport/sse.rs           # :13 / :34
cargo test transport::sse::tests::leading_bom                    # 2 passed
#   变异验证：把 sse.rs:63 改为 `self.consumed += 0;` 后应恰有 3 个变红
#   实测 11 passed / 3 failed（2026-09-15，随后已还原）

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

# T-03 无重连入口
sed -n '28,36p' src/ui/status_bar.rs                            # Lost 仅错误摘要

# T-05 悬空引用
grep -rn "docs/markdown-plan.md" src/                           # render_transcript.rs:330
git log --oneline --diff-filter=D -- docs/markdown-plan.md      # 18463b3

# T-06 offloaded 无消费方
grep -rn "offloaded" src/ | grep -v "offloaded: false" | grep -v protocol/timeline.rs
#   期望：仅 timeline_model.rs 的断言，无生产读取

# §2 回归锁
cargo test transport::http::tests runtime::tests transport::sse::tests

# T-04 变异验证：临时删去 stream_rebuild 的 generation 分支后，应恰好 2 个变红
#   预期失败：generation_bump_rebuilds_stream_and_keeps_cursor、
#             only_unchanged_conn_info_avoids_rebuild
#   2026-09-15 实测：10 passed / 2 failed（随后已还原）
```

> 格式化请定向执行（`rustfmt src/runtime.rs`）：裸跑 `cargo fmt` 会连带改动他人 WIP 与既有文件，本仓惯例是 fmt 不纳入他人 WIP（见 `eae0f79`）。

## 7. 未提交工作树（与本清单的关系）

`59541dd` 已把 4 个既有 WIP（`render_transcript.rs`、`subagent.rs`、`timeline_model.rs`、`protocol/timeline.rs`）+ T-04 的 `runtime.rs` + 本清单合成一个提交（按所有者要求"一个大commit"）。此后工作树仅剩 **T-02 的 `src/transport/sse.rs`**（未提交）。

> 注：T-04 期间一次裸跑 `cargo fmt` 曾把 `src/app/session.rs` 的既有格式改动 10 行（纯 fmt，无语义）。该文件不在本任务范围内，且本仓有「fmt 不纳入他人 WIP」的惯例（`eae0f79`），**已还原至 HEAD**。

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
