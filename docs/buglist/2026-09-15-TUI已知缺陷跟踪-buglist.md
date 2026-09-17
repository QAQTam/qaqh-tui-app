# TUI 缺陷台账 · 闭环索引（原 2026-09-15 清单，2026-09-17 收敛）

> ## ⚠ 未解决事项已迁出
>
> 本文件**不再登记待办**。所有未解决事项统一登记在
> **[`docs/todo/2026-09-17-未解决事项-todo.md`](../todo/2026-09-17-未解决事项-todo.md)**（单一台账，含 checklist）。
>
> 本文件保留两样东西：**已闭环条目的索引**（明细交给 git 历史）与**知情接受的风险项**。
> 首版 321 行的明细见 `git log -p -- docs/buglist/2026-09-15-TUI已知缺陷跟踪-buglist.md`。

## 0. 元信息

| 项 | 值 |
|---|---|
| 首版日期 | 2026-09-15 |
| 收敛日期 | 2026-09-17 |
| 基准代码 | TUI `809e317`（main，工作树干净）；后端 `qaqh-backend` main |
| 判定原则 | **以工作树真实代码为准**。历史文档（report / handoff / 已删除的 `streaming-edge-audit.md`）的自述状态仅作线索，不作依据 |
| 核定环境 | Arch Linux / rustc 1.98.1。本轮复测（**本机**，核定 commit `809e317`）：`cargo test --all-targets` **148 passed / 0 failed**；`cargo clippy --all-targets -- -D warnings` **零 warning**；`cargo fmt --check` **干净**（不加 `--all`——那会连带格式化 `../qaqh-backend` 里别人的工作树）。**⚠ CNB 侧 CI 自 2026-09-15 起不可用，见 todo 台账 U-22** |
| 状态图例 | `DONE` 已核实闭环 ｜ `ACCEPTED` 知情接受 ｜ `INVALID` 已核实不成立 |

### 0b. 本次收敛做了什么

首版 321 行，且项目同时存在**三本互不引用的账**（本文件 + `docs/report/2026-09-14-...-report.md`
+ 仓库根 `BUGLIST.md`）。2026-09-17 的整理动作：

1. 已闭环条目的明细表（原 §2 / §4 / §5b）压缩为下方 §1 的一行式索引；
2. 仓库根 `BUGLIST.md` 归档为 [`2026-09-16-codegraph静态排查-buglist.md`](2026-09-16-codegraph静态排查-buglist.md)；
3. 未解决事项全部迁入 `docs/todo/` 单一台账；
4. 快照数字按当前 HEAD 复测（见 §0 与 §4）。

## 1. 已闭环索引（DONE）

> 一行一条：编号 → 事项 → 落点/提交。明细请查 git 历史。

| 编号 | 事项 | 闭环落点 |
|---|---|---|
| T-01 | 传输层迁移 `qaqh-client`（连接生命周期 / 三频道流 / per-seed timeline） | 阶段一 `56c31e7`、阶段二 `a278ab8`/`1eca0b3`/`868c744`/`b23be65`/`b1a150f`、阶段 1.5 `d379d90`。实测 `src/transport/` 不存在、`protocol/` 141 行纯 re-export、自建轮子符号零命中 |
| T-02 | SSE 流首 BOM 吞首帧 | `8663f82` 修复，随迁移上移 `qaqh-client/src/sse_decoder.rs`（`const BOM` + 一次性 `bom_checked` 守卫，含跨 chunk 等待） |
| T-03 | 无用户可触发的重连入口 | `Ctrl+R`：`src/app/keymap.rs:37/51`、`src/ui/status_bar.rs:31`；真机含失败路径实测 |
| T-04 | D-3 无回归锁 | 落点对端 `qaqh-client/src/sse.rs:207 reconcile_epoch`、`src/timeline.rs:36 observe_epoch` |
| T-05 | 悬空文档引用（`docs/markdown-plan.md` 已删） | `18efff3`；裸魔数 `500` 提为具名常量 `MD_BLOCK_LINE_CAP` |
| T-06 | `offloaded` 字段无消费方 | 后端 `ea6063c` 前提成立 → TUI `d0b9267`（搬运 + 归档预览标注） |
| T-07 | `TurnOpened` 不镜像后端原地 reopen | 补回被丢弃的 `sealed` 字段 + 原地 reopen 分支；双向变异验证 |
| T-08 | 重建窗口型快照的 `has_more` 语义未定 | 语义定案（`has_more` / `total_turns` / `truncated_before` 三分）；后端 `7ef99fb` + TUI `ff38bea` |
| T-09 | `TimelineToolState` 少 `Cancelled`/`Backgrounded` 两个终态 | `a278ab8`（换权威类型 + 补渲染分支，由编译期非穷尽 match 强制） |
| T-10 | `BlockCheckpoint` 的 `arg`/`text` 语义弄反 | `a278ab8`；含一次差点写成的「真吞字」 |
| T-11 | `ConfigPatch` 缺 `permission_level`、`ConfigDto` 缺 `mcp`/`lsp` | 删 367 行手抄件，改依赖权威 crate `qaqh-config-api` |
| T-12 | 仍调用已下线的 `workspace.set_mode` | `868c744`：整个 workspace-mode 功能按「能力已下线」删除 |
| T-13 | 死镜像逃过 `dead_code` lint | 两例均已删（`snapshot.rs` 三类型 + `methods.rs` 38 常量表）。**教训：阶段二删镜像不能指望编译器指出残留，必须逐项 grep 真实调用点** |
| T-14 | `SessionState::meta` 是从未生效的字段 | G2 类型化时一并删除，语义不变 |
| D-1 | 401 三态分类的裁决权 | 上移至 `qaqh-client`；本仓 `is_fatal`/`is_credential` 已删 |
| D-2 | 凭据热更新的实现 | 上移至 `qaqh-client`（`session.rs:108 refresh_discovery`） |
| D-3 | 流感知重协商 | 落点对端（`reconcile_epoch` / `observe_epoch`） |
| D-4 | 终止帧归一 | 落点对端（`ringing.stream_terminated` → `ClientError::Transport`） |
| B1 | 丢弃可观测化 | `timeline_model.rs` 计数 + `status_bar` 非零展示 |
| C3 | 流首 BOM（原 ACCEPTED，被 `BUG-2026-09-13-17` 反证） | 升级为 T-02 并已修。**教训：以「上游不会那样发」为由接受的边界，会被现实证伪** |
| — | 非 bash progress 有界（后端登记册 `O-2`） | `59541dd`；后端登记册已引用本仓提交 |
| — | `BUGLIST.md` BUG-001 / BUG-010（渲染缓存冻结 / `show_reasoning` 未进缓存键） | `77dd2fd`：transcript 分段渲染缓存与虚拟化；顺带修掉缓存键宽度错位与压缩动画冻结 |

## 2. 知情接受（ACCEPTED，非待办）

> 这些是**已判定可接受**的风险，不是未完成项。若前提变化需重新评估。

| 编号 | 项 | 事实 | 维持理由 |
|---|---|---|---|
| B2 | 重复 `BlockOpened` 整块替换 | reducer 走覆盖语义，仍会清空已累计文本 | daemon `DuplicateBlock` 拒绝 + 整流重摆不重放 → 契约上不可达；若未来引入「流内重开块」必须先改此处 |
| C1 | 非法 UTF-8 行整行跳过 | 落点 `qaqh-client/src/sse_decoder.rs`（绝不 lossy） | 设计行为，保护中英文/emoji 完整性 |
| C2 | 单行无界增长 | 对端同款「只搬移已消费前缀」游标设计，**仍无单行熔断** | daemon 可信；防御性上限留待未来 |
| D1 | `is_gerund_word` 对 4 字母 -ing 词误报 | 仅影响首行是否提升为标题 | 无内容丢失 |
| D2 | text/reasoning 原样渲染 `\r` | `apply_bash_progress` 只覆盖 bash 进度 | LLM 正文罕见输出 `\r` |
| D3 | sealed 瞬间纯文本→markdown 切换 | 渲染语义切换 | 非丢字 |
| D4 | 单块 markdown > `MD_BLOCK_LINE_CAP` 截断 | **单块内**截断，且带「（内容省略 N 行）」可见标注 | 设计，防超大块在预折行缓存里成倍放大 |
| E1 | `perf_sse_throughput` 回归锁丢失 | 本仓与 `qaqh-client` 全仓零命中该测试名 | 设计等价（游标无搬移）、风险低；**但若对端将来改成搬移式实现，本仓不会红** |
| E2 | 首屏 timeline 窗口 60 → 30 | 首屏走 daemon 默认 30；`request_rebaseline`/`load_older` 仍用本仓 `TIMELINE_PAGE_LIMIT = 60` | 与 winui 对齐；**但首屏可见回合数变少是用户可见变化** |
| E3 | timeline 激活对 401 的重试有上限 | `ATTACH_RETRY_ATTEMPTS = 75`（≈30s）后放弃并报 `TimelineLost` | 无界重试会泄漏任务 |

## 3. 与后端权威登记册的关系

**权威登记册在后端仓库** `qaqh-backend/docs/buglist/`（登记规则：一行一个缺陷；详情进 `docs/report/`；
状态口径 `open` / `fixed（工作区，待提交）` / `fixed @{commit}` / `verified` / `wontfix`）。
本文件是 TUI 侧补充索引，**不是第二本账**——同一缺陷以后端 ID 为准。

| 本仓条目 | 后端登记册 | 关系 |
|---|---|---|
| T-02（流首 BOM） | `BUG-2026-09-13-17` | 同一缺陷：后端已修，TUI 侧长期未同步（漂移实例） |
| T-06（`offloaded` 无消费方） | `BUG-2026-09-14-03` | 同源（`enable_turn_offload` 死代码） |
| 非 bash progress 有界 | `O-2`，`fixed @59541dd` | 同一改动 |
| D-4（终止帧归一） | `BUG-2026-09-12-11` | 后端已修；TUI 侧 D-4 是其消费端对偶 |
| D-1 / D-2 / D-3（401 族） | `BUG-2026-09-12-10` | **族相关但根因不同**：后端修的是服务端身份与租约；TUI 的 401 卡死是消费端错误分类自杀 |
| — | `BUG-2026-09-17-06` | 后端登记册里的 **TUI 侧**条目；其机制描述与代码不符，真实缺口已并入 todo 台账 **U-01 / U-11** |

> **未完成（已登记为待办）**：后端登记册中上述条目尚未标注「已移交本仓」，两边仍是双份记录
> → todo 台账 **U-12**。

## 4. 快照数字（2026-09-17 复测）

首版多处数字已过期，以本节为准：

| 指标 | 首版记录 | 复测（`809e317`） |
|---|---|---|
| `cargo test` | 124 passed | **148 passed / 0 failed** |
| `src/protocol/` 行数 | 79 | **141**（仅 `mod.rs`，纯 re-export） |
| `src/runtime.rs` 行数 | 431（§9 自述） | **465** |
| `grep -c qaqh Cargo.lock` | 11 | **13** |
| `STALL_AFTER` | §2/§5b.5 记 15s、§9 记 20s（自相矛盾） | **20s**（`src/runtime.rs:54`） |
| `src/transport/` | 318 行 | **目录不存在** |
| 本仓 `cargo fmt --all --check` | §0/§9 称全绿 | 曾在 `src/app/markdown.rs` 有 3 处偏差，已修；现行命令为 `cargo fmt --check` |

> **教训**：台账里的「实测数字」会随提交快速过期。今后数字要么标注核定 commit，
> 要么只保留可复现命令、不再抄写结果。

## 5. 已核实不成立（INVALID）

原 `BUGLIST.md` 与首版清单中若干条目经复测不成立，逐条标注见
[`2026-09-16-codegraph静态排查-buglist.md`](2026-09-16-codegraph静态排查-buglist.md)，
汇总见 todo 台账 §6「已核实**不成立**」。
