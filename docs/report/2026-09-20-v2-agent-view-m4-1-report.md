# QAQH TUI v2 Agent View M4.1 报告

> 状态：**M4.1 Agent inline 外壳与真实事件循环接线已完成**
> 日期：2026-09-20
> 上游报告：[`2026-09-20-v2-transcript-m3-4-report.md`](2026-09-20-v2-transcript-m3-4-report.md)
> 关联规范：
> [`2026-09-20-v2-agent-view-wireframe-spec.md`](../spec/2026-09-20-v2-agent-view-wireframe-spec.md) ·
> [`2026-09-20-v2终端提交协议-spec.md`](../spec/2026-09-20-v2终端提交协议-spec.md)

---

## 0. 结论

M4.1 首次把 V2 transcript runtime 接到真实 `Runtime` / `App` 事件循环：

- 新增实验入口 `--v2-agent` / `QAQH_V2_AGENT`；
- 使用 `Viewport::Inline(10)`，不进入 alternate screen；
- 复用 v1 的 `Runtime`、`AppMsg`、`App` 状态机与全部协议处理；
- 已封口 transcript 经 `V2TranscriptRuntime` 与 commit ledger 幂等写入 scrollback；
- live block、composer、status、shortcuts 只占底部 inline viewport；
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
| Ctrl+N | 新建会话 |
| Ctrl+L | 打开会话列表状态（M4.1 尚未绘制 Workspace/Modal） |
| F1 | 打开帮助状态（M4.1 尚未绘制 Workspace/Modal） |
| Ctrl+R | 手动重连 |
| Ctrl+C×2 / Ctrl+Q | 退出并恢复终端 |

---

## 4. 安全与性能

- composer 的换行、回车和其余控制字符在进入绘制前被替换，禁止 ESC 等控制字节
  直接写入终端；
- 已封口内容先过 `TranscriptCommitLedger`，同 `(seed, turn, block, revision)` +
  同内容最多提交一次；
- commit 按 32 个 block 一批，避免一次 bootstrap 产生过大的临时绘制批次；
- live 渲染只取活动会话的 `BlockState::Live`，并按 inline 高度保留尾部窗口；
- pending 队列上限沿用 M3.4 的 4096，异常事件率不会造成无界增长；
- 不引入新依赖、不修改协议、不修改后端锚点。

---

## 5. 测试

新增：

```text
terminal::agent::tests::first_snapshot_replays_once_then_syncs_incrementally
terminal::agent::tests::agent_render_keeps_composer_visible_on_narrow_cjk_input
terminal::agent::tests::agent_render_sanitizes_control_characters
terminal::agent::tests::inline_agent_draw_survives_resize
```

覆盖：

- 首次权威快照重放一次，后续增量不重复；
- 20 列窄屏 + 中文长输入时光标仍在 viewport 内；
- 原始 ESC 控制字节不进入终端；
- 80×24 → 40×20 → 20×8 → 120×40 连续 resize 不崩。

验证结果：

```text
cargo fmt --check                            # 通过
cargo clippy --all-targets -- -D warnings    # 通过
cargo test --all-targets                     # 288 passed / 0 failed / 6 ignored
```

真实 PTY 冒烟受当前环境没有 live daemon 限制：`--no-spawn` 路径在进入 inline
viewport 前返回清晰的“连接 daemon 失败”，未污染 scrollback。完整端到端冒烟在
daemon 可用的验收环境执行。

---

## 6. 已知边界

- Workspace / Modal / 会话选择器 / 设置 / 权限 / ask / plan 尚未绘制，M5 接入；
- 首帧没有活动会话时只显示新建/列表快捷键提示；
- 分页加载更早回合的 scrollback 插入顺序尚未定义，M6 处理；
- 动态 viewport 高度、slash 菜单和附件提示尚未迁移；
- 仅实验入口启用，v1 仍是默认路径。

---

## 7. 下一步

**M4.2：Composer / Status / Shortcuts 完整化**：

- composer 多行窗口、光标和 slash 菜单；
- status 的模型、cwd、连接告警与 usage；
- 动态 shortcuts；
- live tool / thinking 的流式更新与失败内联；
- 为 M4 增加 PTY 驱动验收。
