# QAQH TUI `betav2` 封仓 handoff（2026-09-23）

> 封仓时间：2026-09-23 23:5x
> 收口 PR：**#47**（`betav2 → main`，8 提交 / 9 文件）
> 分支纪律：**`betav2` 自此不再前进**；自 2026-09-24 00:00 起开发主线为 `main`

## 0. 一句话

`betav2` 作为 v2 开发线的使命结束——它把 v2 Agent View（终端 scroll 架构）、
交互与故障 harness、真实终端矩阵、性能与静态门禁全部做完并合入 `main`。
**剩余工作集中在「等后端 v2 实现」与「M7 切换默认」两块**，见 §4。

## 1. 封仓基线

| 项 | 值 |
|---|---|
| `betav2` tip | `f5b46dc` |
| 版本 | `2.0.0-alpha1` |
| 后端锚点 | `tui-ringing-v2-frozen-2026-09-23` @ `b40ff698`（annotated，不可移动） |
| 协议 | Ringing **v1**（v2 语义已冻结，实现待后端） |
| 默认启动模式 | **v1 全屏**（`--v2-agent` / `QAQH_V2_AGENT=1` 进 v2） |
| 收口 PR | **#47** |

## 2. 封仓时的能力面（已验收）

**渲染 / 架构**

- v2 Agent View = **终端 scroll**：`Viewport::Inline` + 封口块经 `insert_before`
  写进终端原生 scrollback + commit ledger；alternate screen 仅 Workspace/Modal 临时进出；
  **不开鼠标捕获**（保留原生选择/复制）。
- v1 全屏路径完整保留（默认仍是它）——含 `TranscriptCache` / 估算 / 淘汰 / 视口不动点。

**交互与故障 harness**

- `e2e-v2-interactions.sh` **9 个 MODE 全绿**：`permission` / `ask` / `plan` / `pager` /
  `permission-hang` / `ask-hang` / `permission-deny` / `ask-dismiss` / `plan-reject`。
  否定路径用**后端权威证据**判定（timeline 物化 + provider 上下文配对 + 会话事实账本指纹）。
- `e2e-v2-faults.sh` 六个模式：**带修复版 daemon 全绿**；默认锚点下 5/6 红（原因见 §3.3）。

**真实终端矩阵**（`docs/spec/2026-09-21-v2-terminal-compatibility-matrix.md` §3）

- Kitty ✅（render / truecolor / resize / scrollback / exit，10/10）
- tmux ✅（scrollback `capture-pane -S -` / resize / `mouse_any_flag=0`，10/10）
- WezTerm ✅（9/9）；Alacritty ⚠️ 进程级（无 IPC）；其余终端按 §3.5 **决定不逐个支持**
- 环境能力矩阵 12/12 profile 全绿（含 `mouse=off`）

**门禁（全部接进 `scripts/ci-linux.sh`）**

- `cargo fmt --check` / `clippy --all-targets -D warnings` / `test --all-targets`
  （**337 passed / 0 failed / 9 ignored**）
- `scripts/perf-gate.sh` —— M6.4 性能门禁，钉结构性性质（含证伪记录）
- `scripts/static-gates.sh` —— 跨仓契约静态门禁 **G1–G4**（含白名单与逐条证伪）

**跨仓**

- 锚点机制：`ci-linux.sh::QAQH_BACKEND_REV` + 本机 `.cargo/config.toml` path override
- 后端缺陷已迁到**后端仓**：`qaqh-backend#314`（timeline seq + seal 裁剪）、
  `#315`（session-title panic）——**两条后端都已修复合入**（见 §3.4）

## 3. 已知问题（封仓时未清）

### 3.1 锚点 worktree 被后端收束删除 → **已重建，但 daemon 未构建**（按决定推迟）

后端做 worktree 收束时删掉了 `../qaqh-backend-anchor`（TUI 的锚点检出），
当时 `cargo check` 直接失败：

```
error: failed to update path override
  `/home/qaqtamsy/项目/qaqh-backend-anchor/crates/qaqh-client`
  (defined in `/home/qaqtamsy/项目/qaqh-tui-app/.cargo/config.toml`)
```

**当前状态（封仓时）**：

| 项 | 状态 |
|---|---|
| 锚点 worktree | ✅ **已重建**于冻结 tag `b40ff698` |
| `cargo check/build`（TUI） | ✅ 可用（path override 解析正常） |
| `qaqh-daemon` 二进制 | ❌ **未构建** → 所有 e2e 脚本会以「缺少可执行文件」失败 |

**恢复命令**（alpha1 正式开工时执行，一条即可）：

```bash
cd ../qaqh-backend-anchor && cargo build -p qaqh-daemon --bin qaqh-daemon
```

> 若 worktree 再次被删（后端再收束），先重建：
> ```bash
> git -C ../qaqh-backend worktree add --detach ../qaqh-backend-anchor \
>   b40ff698f4211526f139c8a620cf159dd4ef9542
> ```
>
> 注：`.cargo/config.toml` **不入库**（gitignore）；仓库 CI 的路径由
> `ci-linux.sh::prepare()` 自己拉 `../qaqh-backend` 到钉死 rev，不依赖这个 worktree。
> 也就是说：**这个 worktree 只影响本机 e2e，不影响 CI 与仓库门禁。**

### 3.2 `main` 的 CI 是红的（**平台层，非代码**）

`a4b5c58` 的 2 条 pipeline 都在**进入代码检查之前**失败：runner 日志里没有任何
`cargo test` / `ci-linux.sh` 输出，可见步骤全是 docker 拉取/清理且 exit 0，
只有 teardown 报 `docker kill …dind-proxy: No such container`。
与 #288 那次「Prepare 阶段失败」同形态，需平台侧跟进。**本仓代码侧无解。**

### 3.3 故障套件在**默认锚点**下 5/6 红（预期）

`none` / `lagged` / `gap` / `ack-delay` / `ack-hang` 都断言「回复可见」，
依赖后端 #314 修复；`session-404` 不依赖，绿。脚本头已写明口径，**不要误判成 TUI 回归**。

### 3.4 后端已修 #314 / #315，但**尚未发新冻结 tag**

- `bfc4270`「合并 fix/314-timeline-seq-gap（PR #318）」——**内容即我定位的两处修复**
  （`timeline_hub.rs` 的 `None` 分支加 `turn_count > 0` 门槛 + `timeline.rs` 去掉
  seal 即时裁剪），另加 `hub.rs` / `persistence_policy.rs` / `timeline_rebuild.rs` 加固；
- `85c8c7b`「合并 fix/315-sync-eager-send（PR #319）」——按建议修法
  （把 `.send()` 挪进 `block_on(async { … })`）改 `chat_completions_api.rs` / `message_api.rs`。

**但冻结 spec 规定「任何改变 cursor / reset / replay / interaction / driver 语义的修改，
后端会另起冻结 tag」**——#314 改的正是 replay/seq 语义。所以 TUI 侧**应等新 tag**，
而不是把锚点指向会移动的 `betav2` tip。

## 4. 接下来要做的工作

### A. 立即可做（不依赖后端）

1. **M7：切换默认到 v2 + 清理 v1 滚动路径 + 发布文档**
   - `src/main.rs::select_startup_mode` 默认改为 v2 Agent View；
   - `--v1` 保留为**显式回退闸**（语义不变）；
   - 清理 v1 全屏专有机制（`TranscriptCache` / `estimates` / `BlockSeg` /
     `keep_turn_range` / `refresh_at_viewport`）——已核：**v2 零引用**，可安全移除；
   - **同时是 v1→v2 协议切换点**（见 §5）。
2. **修复 §3.1 的锚点 worktree**（alpha1 开工第一步）。
3. 可选：`render` 基准进一步拆分（live viewport 内部：markdown / tool 卡占比）。

### B. 等后端**新冻结 tag**

4. 换锚点（`ci-linux.sh` + `.cargo/config.toml`）→ 重跑六个故障模式，
   预期**默认锚点下全绿**；随后删掉 `e2e-v2-faults.sh` 头部的「预期红」注记。
5. 换锚点后复跑：9 个交互 MODE + 终端矩阵 + 两个门禁。

### C. 等后端 spec §14 步骤 1–2（**TUI 的前置**）

后端冻结 spec 的实现顺序：

```
1. qaqh-ringing 增加 v2 wire 类型、cursor token 编解码和 schema 常量
2. qaqh-client 增加 v2 open/subscribe/bootstrap/reset/interaction API   ← TUI 消费面
```

后端在 issue #46 里写明「**代码：尚未开工**」。**类型不到，落代码就是白写。**

6. **issue #46 task 2**：SessionModel v2 契约——`server_epoch` / `log_id` / opaque `cursor` /
   `last_fact_seq` / `last_projection_index` / `state_revision` / `pending_interactions` /
   `driver_holder` / `driver_epoch`；reducer 四条规则（reliable 按
   `(fact_seq, projection_index)` 推进、replaceable 按 revision 覆盖且不推进 cursor、
   ephemeral 不入持久态、reset 先验证新 bootstrap 再原子替换）；旧 epoch/log/response
   不得回滚新状态；`ringing.reset_required` 是**正常 rebaseline，不是 toast**。
7. **task 3**：interaction 幂等——按 `interaction_id` 去重、first-answer-wins、
   重复 `InteractionRequested`/`InteractionResolved` 幂等、reconnect 以 bootstrap 的
   pending set 为准、**command ack ≠ interaction 完成**（须等 `causation_id = command_id`
   的 reliable 事件）。
8. **task 4**：driver capability——非 driver 的 composer/cancel/undo/workspace 进只读态；
   非 driver 仍可订阅与回答 interaction；driver 状态只从 bootstrap/`DriverChanged` 更新；
   **不本地猜 holder、不把 lease 当 driver**；`driver_epoch` 变化后旧控制命令不得显示可成功。
9. **task 5**：v2 fixture 与验收矩阵 `V2-C1..C7` / `V2-R1..R4` / `V2-D1..D3` / `V2-V1` /
   `V2-T1/T2` / `V2-W1`。
   - `V2-V1`（v1 cursor 服务端映射）**不在 TUI 实现**——由 daemon 负责。

### D. Windows alpha

10. 共用 issue #46 的语义与 fixture，**不单独分叉协议**（spec §12）。

## 5. 协议切换点（M7 同时是它）

冻结 spec §0 裁决 1：**「v2 是 beta 起的权威重连协议；v1 继续保留，但只作为 2.0 兼容面」**。

所以 M7 的「切换默认」**不只是切渲染模式**：beta 默认 v2，v1 只作 2.0 兼容回退。
**v1 → v2 cursor 映射由 daemon 负责，TUI 不实现自己的映射**（spec §10 / §15）。

## 6. 验证方式（新接手人先跑这三条）

```bash
# ① 仓库门禁（含性能门禁 + 静态门禁）
bash scripts/ci-linux.sh

# ② 交互九模式（需 daemon；默认吃锚点 worktree）
for m in permission ask plan pager permission-hang ask-hang \
         permission-deny ask-dismiss plan-reject; do
  MODE=$m bash scripts/e2e-v2-interactions.sh
done

# ③ 真实终端矩阵（本机已装 kitty / tmux / wezterm / alacritty）
bash scripts/e2e-v2-real-terminal.sh   # kitty
bash scripts/e2e-v2-tmux.sh            # tmux
bash scripts/e2e-v2-wezterm.sh         # WezTerm
bash scripts/e2e-v2-alacritty.sh       # Alacritty（PASS(partial) 属预期）
```

## 7. 关键文档索引

| 主题 | 文档 |
|---|---|
| v2 章程与里程碑 | `docs/plan/2026-09-20-v2视觉与交互重构-plan.md`（§5.2 里程碑 + 分支/协议切换点） |
| 终端兼容矩阵 | `docs/spec/2026-09-21-v2-terminal-compatibility-matrix.md`（§3 实测、§3.5 范围决定） |
| M6.4 性能 | `docs/report/2026-09-21-v2-m6-4-performance-baseline-report.md`（§10 门禁、§11 render 拆分） |
| M6.3 PTY 硬化 | `docs/report/2026-09-21-v2-m6-3-pty-hardening-report.md` |
| 后端 #42 根因与修复 | `docs/report/2026-09-23-backend-issue42-root-cause-and-fix-report.md` |
| 锚点刷新（v2.0.0 RC） | `docs/report/2026-09-23-backend-anchor-v2.0.0-rc-report.md` |
| 交互否定路径 | `docs/report/2026-09-23-v2-interaction-negative-paths-report.md` |
| 对后端的协作需求 | `docs/spec/2026-09-23-TUI对后端的协作需求-spec.md`（§7 汇总表） |
| **v2 冻结语义（后端）** | 后端 `docs/spec/2026-09-23-TUI-Ringing-v2冻结语义-spec.md`（tag `tui-ringing-v2-frozen-2026-09-23`） |
| 契约测试钩子（后端） | 后端 `docs/spec/2026-09-23-TUI契约测试钩子-spec.md` |

## 8. issue / PR 现状

| 编号 | 位置 | 状态 |
|---|---|---|
| **#47** | 本仓 PR | **open / mergeable** —— `betav2 → main` 收口，待合 |
| **#46** | 本仓 issue | open —— Ringing v2 冻结语义消费；task 1 ✅ / task 6 ✅ / task 2–5 ⛔ 等后端 |
| #45 | 本仓 issue | open —— session-title panic（**后端已修** `85c8c7b`，TUI 侧跟踪） |
| #42 | 本仓 issue | open —— timeline seq 缺口（**后端已修** `bfc4270`，TUI 侧跟踪） |
| #44 / #41 | 本仓 issue | open —— TUI 侧消费任务，均已完成，待后端确认后关 |
| **#314 / #315** | **后端仓** | open —— 后端已修复合入（`bfc4270` / `85c8c7b`），待其关闭并出**新冻结 tag** |
