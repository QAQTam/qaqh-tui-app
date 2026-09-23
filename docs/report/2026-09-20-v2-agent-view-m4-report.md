# QAQH TUI v2 Agent View M4 报告

> 状态：**M4 Composer / Status / Shortcuts 已完成**
> 日期：2026-09-20
> 上游报告：[`2026-09-20-v2-transcript-m3-4-report.md`](2026-09-20-v2-transcript-m3-4-report.md)
> 关联规范：
> [`2026-09-20-v2-agent-view-wireframe-spec.md`](../spec/2026-09-20-v2-agent-view-wireframe-spec.md) ·
> [`2026-09-20-v2终端提交协议-spec.md`](../spec/2026-09-20-v2终端提交协议-spec.md)

---

## 0. 结论

M4 把 V2 transcript runtime 接到真实 `Runtime` / `App` 事件循环，并完成默认
Agent View 的输入、状态与流式交互底座：

- 新增实验入口 `--v2-agent` / `QAQH_V2_AGENT`；
- 使用固定 `Viewport::Inline(10)`，不进入 alternate screen（M6.1 已改为动态高度）；
- 复用 v1 的 `Runtime`、`AppMsg`、`App` 状态机与全部协议处理；
- 已封口 transcript 经 `V2TranscriptRuntime` 与 commit ledger 幂等写入 scrollback；
- live block、composer、status、shortcuts 只占底部 inline viewport；
- composer 支持多行、宽字符折行、尾部窗口与光标可见；
- slash 输入显示一级命令菜单，选择状态与宽度降级可用；
- status 展示连接、活动、model、mode、cwd、usage、附件和连接告警；
- live thinking / tool 卡沿用 M3 renderer，在活动回合内持续更新；
- 不启用鼠标捕获，保留终端原生选择/复制；
- v1 默认全屏路径不变。

---

## 1. 模块边界

```text
Runtime / App
  ├─ 连接、timeline、命令、交互状态（既有唯一事实源）
  └─ active session.timeline.turns
           │
           ▼
terminal::agent::AgentState
  ├─ 首次进入 seed：reset_from_turns + replay_all
  └─ 后续：sync_turn 增量投影
           │
           ▼
V2TranscriptRuntime / CommitLedger
  └─ 只返回首次 Emit 的 PendingCommit
           │
           ▼
Terminal::insert_before
  └─ 已提交内容进入 scrollback
```

inline viewport 不保存历史，也不承担 v1 的滚动窗口、高度估算或离屏淘汰。

---

## 2. 启动与退出

```bash
cargo run -- --v2-agent
cargo run -- --v2-agent --no-spawn
```

- 启动前仍走 `Runtime::start`，失败时在进入 inline viewport 前返回；
- 启动成功后启用 bracketed paste，不启用 mouse capture；
- 退出时先关闭 bracketed paste，再 shutdown runtime，最后 `ratatui::restore()`；
- `Ctrl+C` 双击 / `Ctrl+Q` 沿用 App 既有退出语义。

---

## 3. 已实现交互

| 输入 | 行为 |
|---|---|
| 字符 / Backspace / 方向键 | 复用 v1 composer 编辑状态 |
| Enter | 复用 v1 发送消息路径 |
| Alt+Enter / Ctrl+J | 多行输入 |
| `/` | 显示命令候选，↑↓ 选择、Tab/Enter 补全 |
| Ctrl+N | 新建会话 |
| Ctrl+L | 打开会话列表状态（绘制由 M5 Workspace 接管） |
| F1 | 打开帮助状态（绘制由 M5 Workspace 接管） |
| Ctrl+R | 手动重连 |
| Ctrl+C×2 / Ctrl+Q | 退出并恢复终端 |

---

## 4. 视觉与布局

- 底部固定 status + shortcuts 两行；
- composer 最多占 4 行，slash 菜单最多占 4 行；
- 剩余空间全部留给 live transcript；
- 窄屏优先保留 composer、status 与快捷键，再压缩 slash/live；
- slash 菜单在 <50 列时隐藏描述，只保留命令名与选择标记；
- status 在 40 / 50 / 70 列设三档信息预算，避免窄屏把关键状态挤掉；
- Workspace / Modal 尚未绘制时，在 live 区显示明确的 M5 过渡提示，避免“按键后
  完全无反馈”的隐状态。

---

## 5. 安全与性能

- composer 的换行、回车和其余控制字符在进入绘制前被替换，禁止 ESC 等控制字节
  直接写入终端；
- 宽字符按 `unicode-width` 折行，光标列不会落入宽字形中间；
- 已封口内容先过 `TranscriptCommitLedger`，同 `(seed, turn, block, revision)` +
  同内容最多提交一次；
- commit 按 32 个 block 一批，避免一次 bootstrap 产生过大的临时绘制批次；
- live 渲染只取活动会话的 `BlockState::Live`，并按 inline 高度保留尾部窗口；
- pending 队列上限沿用 M3.4 的 4096，异常事件率不会造成无界增长；
- 不引入新依赖、不修改协议、不修改后端锚点。

---

## 6. 测试

新增：

```text
terminal::agent::tests::first_snapshot_replays_once_then_syncs_incrementally
terminal::agent::tests::agent_render_keeps_composer_visible_on_narrow_cjk_input
terminal::agent::tests::agent_render_sanitizes_control_characters
terminal::agent::tests::composer_rows_split_newlines_and_track_cursor
terminal::agent::tests::composer_rows_wrap_wide_text_and_keep_cursor_visible
terminal::agent::tests::slash_menu_tracks_selection_and_stays_in_viewport
terminal::agent::tests::live_thinking_and_tool_cards_render_in_agent_viewport
terminal::agent::tests::agent_delegates_enter_to_existing_composer_send_path
terminal::agent::tests::inline_agent_draw_survives_resize
```

覆盖：

- 首次权威快照重放一次，后续增量不重复；
- 20 列窄屏 + 中文长输入时光标仍在 viewport 内；
- 多行换行、宽字符折行与尾部窗口；
- slash 选中项在菜单窗口内；
- live thinking / running tool 在 viewport 内可见；
- Enter 仍走 v1 发送路径并清空 composer；
- 原始 ESC 控制字节不进入终端；
- 80×24 → 40×20 → 20×8 → 120×40 连续 resize 不崩。

验证结果：

```text
cargo fmt --check                            # 通过
cargo clippy --all-targets -- -D warnings    # 通过
cargo test --all-targets                     # 293 passed / 0 failed / 6 ignored
scripts/tests/ci-linux-parse-test.sh         # 15 passed / 0 failed
```

真实 PTY 冒烟：

- live `qaqh-daemon` 启动后，`--v2-agent --no-spawn` 能进入 inline viewport；
- `Ctrl+N` 触发真实会话创建，daemon session index 有对应 seed；
- `Ctrl+Q` 退出后 bracketed paste、alternate screen 状态与光标恢复；
- 环境无 live daemon 时，在进入 inline viewport 前返回清晰错误，不污染 scrollback。

---

## 7. 已知边界

- Workspace / Modal / 会话选择器 / 设置 / 权限 / ask / plan 的完整绘制由 M5 接入；
- M4 阶段 inline viewport 固定 10 行，内部布局自适应；动态高度已在
  [`2026-09-20-v2-dynamic-inline-viewport-m6-1-report.md`](2026-09-20-v2-dynamic-inline-viewport-m6-1-report.md) 完成；
- 分页加载更早回合的 scrollback 插入顺序尚未定义，M6 处理；
- 仅实验入口启用，v1 仍是默认路径。

---

## 8. 下一步

**V2-M5：Workspace / Modal 接管管理面**：

- 会话选择器、设置、帮助；
- 权限、ask_user、plan review；
- workspace / todo / 子代理观测；
- 退出 Workspace/Modal 后返回 Agent View，且不污染 scrollback。
