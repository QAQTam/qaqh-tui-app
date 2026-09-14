# TUI 已知缺陷跟踪清单（以代码为准）（2026-09-15）

## 0. 元信息

| 项 | 值 |
|---|---|
| 清单日期 | 2026-09-15 |
| 基准代码 | `main` @ `ba26c8f`（工作树另有 5 个未提交改动：4 个既有 WIP + 本清单 T-04 闭环的 `src/runtime.rs`，见 §7） |
| 判定原则 | **以工作树真实代码为准**。每条状态由 `file:line` + 可复现命令核定；历史文档（report / handoff / 已删除的 `streaming-edge-audit.md`）的自述状态仅作线索，不作依据 |
| 核定环境 | Arch Linux 7.2.4 / rustc 1.98.1；`cargo test` = **160 passed / 0 failed**（T-04 闭环后 +5）；`cargo clippy --all-targets` = **零 warning** |
| 命名约定 | `docs/buglist/以yyyy-mm-dd-标题-buglist.md作为命名` |
| 状态图例 | `OPEN` 待修 ｜ `FIXED` 已核实修复（附回归锁）｜ `ACCEPTED` 知情接受 ｜ `EXTERNAL` 他仓/环境，本仓不可核实 |

## 1. 待办（OPEN）

| ID | 级别 | 项 | 代码事实（核定证据） | 影响 | 建议动作 |
|---|---|---|---|---|---|
| T-01 | P1 | 传输层未迁移 `qaqh-client` | `Cargo.toml` 依赖表零 `qaqh-*`（`grep -c qaqh Cargo.lock` = 1，且该命中的是 `qaqh-tui` 包名自身，`Cargo.lock:1518`）；平行实现实测 **4462 行**（`transport/` 1094 + `protocol/` 2419 + `runtime.rs` 949），较报告所称 ~3941 行**已继续增长**（本清单 T-04 又加了 112 行） | 后端每修一个传输缺陷，TUI 需人工追平；漂移已实际发生（BUG-2026-09-13-17 BOM） | 按 `docs/plan/2026-09-14-TUI传输层迁移qaqh-client-plan.md` 执行阶段一（阶段二阻塞于后端服务面枚举缺口） |
| T-02 | P1 | SSE 缺流首 BOM 剥离 | `src/transport/sse.rs:69` 直接 `line.strip_prefix("id:")`，无任何剥离；全文件 `grep -i "bom\|feff"` **零命中** | 中间层注入 BOM 时首行 `id:`/`event:` 前缀失配 → 丢首帧、cursor 不推进 | 解码前剥离首个 `\u{feff}`，补回归测试（如 `bom_prefixed_first_field_is_parsed`） |
| T-03 | P2 | 无用户可触发的重连入口 | `src/ui/status_bar.rs:28-36`：`Lost` 相位仅渲染 `✗ lost` + `truncate_str(err, 30)`；全仓重连语义只有 runtime 内部 `SupervisorAction::Reconnect`（`runtime.rs:162`），**无任何按键到 `client.open()` 的路径** | 一旦进入 `Lost`，用户唯一手段是重启进程 | `Lost` 相位挂按键触发显式重新协商，并在状态栏显示按键提示 |
| T-05 | P3 | 悬空文档引用 | `src/app/render_transcript.rs:330` 注释指向 `docs/markdown-plan.md`，该文件已于 `18463b3 clean docs` 删除（`git log --diff-filter=D` 可证） | 注释把读者引向不存在的设计文档 | 改指向现存文档或删除该引用 |

> **T-04 已于 2026-09-15 闭环**：D-3 的判定抽为纯函数 `stream_rebuild`，两条流共用，由 5 个单测锁定（见 §2）。此后待办为 T-01 / T-02 / T-03 / T-05。

## 2. 已核实修复（FIXED）

| 报告 ID | 项 | 代码落点（实测） | 回归锁（实测通过） |
|---|---|---|---|
| D-1 | 401 三态分类、致命判定收敛 | `http.rs:32` `PLAIN_UNAUTHORIZED`、`:178` 按 body 区分三态、`:63` `is_fatal`、`:68` `is_credential`；`runtime.rs:176` `supervisor_action` 纯函数（`:162` 枚举） | `transport::http::tests::plain_lease_expired_is_lease_required_not_unauthorized`、`plain_unauthorized_is_not_fatal`、`unsupported_version_is_the_only_fatal_case`；`runtime::tests::lease_expiry_must_reconnect_not_stop`、`no_non_fatal_error_ever_stops`、`only_protocol_drift_stops_the_lifecycle` |
| D-2 | 凭据热更新 | `http.rs:77-78` `RwLock<String>`、`:110` `apply_discovery`、`:144` `token()`（clone 出锁避免 `!Send`）；`runtime.rs:193` `refresh_credentials` | `transport::http::tests::apply_discovery_hot_swaps_credentials` |
| D-3 | 流感知重协商 | `runtime.rs:339` `stream_rebuild` 纯函数（`:320` 枚举）：epoch 变 → `ResetAndRebuild`（归零）；仅 generation 变 → `Rebuild`（**保留 cursor**）；都不变 → `None`。两条流共用：`:480`（频道流）、`:703`（timeline 流），原先两处内联判定已消除 | `runtime::tests::generation_bump_rebuilds_stream_and_keeps_cursor`、`epoch_change_resets_cursor_and_rebuilds`、`epoch_change_wins_over_generation_bump`、`unrelated_conn_info_change_does_not_rebuild`、`only_unchanged_conn_info_avoids_rebuild`（5 个；**变异验证**见 §6） |
| D-4 | 终止帧归一 | `runtime.rs:316` `STREAM_TERMINATED`、`:355` `stream_terminated_code`、`:369`（频道流）/`:723`（timeline 流）拦截 | `runtime::tests::termination_frame_forces_channel_reconnect`、`stream_terminated_code_parsing_is_total` |
| B1 | 丢弃可观测化 | `timeline_model.rs:307`/`:310` 计数、`:320` `dropped_summary`、`:378`/`:515` 计数点；`status_bar.rs:80` 非零时展示 | `timeline_model` 内 3 处断言（`:1427`、`:1449`、`:1484` 断言重放不计入） |

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

> **C3（流首 BOM）原列 ACCEPTED**，判据是"服务端不发送、现实中不可达"。该判据已被 `BUG-2026-09-13-17` 反证 → **升级为 OPEN，即 T-02**。

## 4. 本仓不可核实（EXTERNAL）

| ID | 项 | 说明 | 本仓可做的动作 |
|---|---|---|---|
| D-7 / O-3 | 后端 `rebase/76` HEAD 编译失败（`qaqh-runtime/src/ringing/hub.rs:308`，11 error） | 他仓（`qaqh-backend`），本仓无该代码 | 阻塞联调；后端修复后需回归本清单 §2 |
| O-4 | daemon 日志 0 字节（`axum_server.rs:113` 只 `log::warn!`） | 他仓 + 运行环境 | 需后端排查，否则事故无法定案 |
| O-6 | 线上部署 `e61efe07`（2026-09-12）未含分片修复 | 部署环境 | 随下个构建解决 |
| A1 | seal 前补最终 checkpoint | 后端 `c85bb43` 已实施（文档自述，本仓无代码可核） | 无 |
| BUG-2026-09-12-11 | Lagged 终止帧 | 后端侧编号；TUI 侧对偶项 D-4 已核实修复 | 无 |

## 5. 本轮核定纠正的失真

1. **D-5 状态错误（已纠正）**：`docs/report/2026-09-14-TUI连接生命周期401卡死-report.md:21` 将其标为"已修复"，但**同报告 §7 写"本轮未修"**、handoff 的"已知遗留"表也列未修、代码实测同样未修——四处中三处与表格矛盾。本清单改判为 `OPEN`（T-02）。
   → **待办：把该报告第 21 行的状态改为"待办"**（属他人报告的历史记录，本次未擅自改动）。
2. **`streaming-edge-audit.md` 的 C3 判据失效**：原判"现实中不可达"被 `BUG-2026-09-13-17` 反证，已升级为 T-02。
3. **该审计文档本身已不存在**：`18463b3 clean docs` 删除，仅存于 git 历史（`git show 18463b3^:docs/streaming-edge-audit.md`）。本清单 §3 已把其中仍有效的 ACCEPTED 项接管过来，避免随文件消失。

## 6. 核验命令（可复现）

```bash
cd ~/Projects/qaqh-tui-app

# 全局基准
git log -1 --format='%h %ad' --date=short                      # ba26c8f 2026-09-14
cargo test 2>&1 | grep -E "^test result"                        # 155 passed; 0 failed
cargo clippy --all-targets                                      # 零 warning

# T-01 未迁移
grep -c qaqh Cargo.lock                                         # 1（qaqh-tui 包名自身）
wc -l src/transport/*.rs src/protocol/*.rs src/runtime.rs | tail -1   # 4350

# T-02 BOM
grep -in "bom\|feff" src/transport/sse.rs                       # 零命中
sed -n '69p' src/transport/sse.rs                               # strip_prefix("id:")，无剥离

# T-03 无重连入口
sed -n '28,36p' src/ui/status_bar.rs                            # Lost 仅错误摘要

# T-04 D-3 无测试
grep -n generation src/runtime.rs | grep -i "assert\|test"      # 零命中

# T-05 悬空引用
grep -rn "docs/markdown-plan.md" src/                           # render_transcript.rs:330
git log --oneline --diff-filter=D -- docs/markdown-plan.md      # 18463b3

# §2 回归锁
cargo test transport::http::tests runtime::tests

# T-04 变异验证：临时删去 stream_rebuild 的 generation 分支后，应恰好 2 个变红
#   预期失败：generation_bump_rebuilds_stream_and_keeps_cursor、
#             only_unchanged_conn_info_avoids_rebuild
#   2026-09-15 实测：10 passed / 2 failed（随后已还原）
```

> 格式化请定向执行（`rustfmt src/runtime.rs`）：裸跑 `cargo fmt` 会连带改动他人 WIP 与既有文件，本仓惯例是 fmt 不纳入他人 WIP（见 `eae0f79`）。

## 7. 未提交工作树（与本清单的关系）

`git status --porcelain` 显示 5 个文件未提交：4 个既有 WIP（`render_transcript.rs`、`subagent.rs`、`timeline_model.rs`、`protocol/timeline.rs`）+ 本清单 T-04 闭环的 `src/runtime.rs`（+119/−7）。

> 注：T-04 期间一次裸跑 `cargo fmt` 曾把 `src/app/session.rs` 的既有格式改动 10 行（纯 fmt，无语义）。该文件不在本任务范围内，且本仓有「fmt 不纳入他人 WIP」的惯例（`eae0f79`），**已还原至 HEAD**。

**本清单基于该工作树核定**。其中 `timeline_model.rs` 的改动触及 B1 计数所在文件，提交前需复核 §2 中 B1 的回归锁仍通过（现测为通过）。

## 8. 变更记录

| 日期 | 变更 |
|---|---|
| 2026-09-15 | 首版：以 `ba26c8f` 工作树代码为准核定 D-1~D-7 / O-1~O-6 / B1~B2 / C1~C3 / D1~D4 / BUG-2026-09-12-11 / BUG-2026-09-13-17；新开 5 项待办 T-01~T-05；纠正 D-5 状态失真 |
| 2026-09-15 | **T-04 闭环**：D-3 判定抽为 `stream_rebuild` 纯函数（`runtime.rs:339`），两条流共用消除内联漂移；+5 单测（变异验证 10 passed / 2 failed）。测试总数 155 → 160 |
