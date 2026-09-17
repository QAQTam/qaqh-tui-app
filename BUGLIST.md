# Buglist — qaqh-tui-app

> 范围：仅通过 CodeGraph/源码静态排查，未读取 docs。  
> 状态标记：`[confirmed]` 有明确代码路径；`[suspicious]` 有明显反常但未复现；`[unverified]` 推测，需进一步验证。

---

## 已确认问题

### BUG-001 `screenshots/render_spotlight` 损坏：冻结 / 不滚动 / 工具区块丢失 `[confirmed]`

位置：
- `src/app/render_transcript.rs:214` 附近
- `src/app/session.rs:357`（`RenderedTranscript`）
- `src/app/mod.rs:1446`（`ensure_render_caches`）
- `src/ui/transcript.rs:21`（`draw`）

要点：

1. `ensure_render_caches` 使用 `sess.timeline.version` 作为缓存键，但同一回合内容增长会刷新缓存，`version` 未同步 bump 时缓存不会失效，导致“打字到一半停止”的冻结感。
2. `RenderedTranscript` 完全没有“生成时刻”信息，`PageUp` / 尾部条件计算在内容增长后不可靠：高度变化时渲染上下文会漂移出屏。
3. 大量代码块/工具块在 markdown 渲染后直接 `truncate` 超容量，导致同一回合中后面的工具块被整块挤掉；没有分页回退或提示。

建议：
- `RenderedTranscript` 增加内容哈希 + 每块渲染上限与超出端点回退；
- `ensure_render_caches` 增加 fallback：当 `cached` 的 `top` 超出宽高时强制重渲染。

---

### BUG-002 卡片/overlay 层次问题：关闭弹窗后 composer 键可能落到错误面板 `[suspicious]`

位置：
- `src/app/overlay_ops.rs:125` 起
- `src/ui/mod.rs:29`  
- `src/app/mod.rs:1406`

要点：
- `overlay_key` 全量消费按键，但 renderer 与 `App` 层对 `top overlay` 的判断不一致时（多个 overlay 并存 + tab 切换后仍保留 overlay），composer 输入会进入未显示的会话。

建议：
- 关闭 tab / 切换 tab / inspection 时显式清掉不属于该 seed 的 overlay。

---

### BUG-003 新会话 / 创建失败后返回的 toasts 与列表处捕获不完整 `[confirmed]`

位置：
- `src/app/session_ops.rs:69`
- `src/app/mod.rs:353`

要点：
1. `pending_creates` 用 command_id 关联；一旦命令未达状态，UI 需要 15 秒后超时，窗口期内用户没有任何反馈（无“创建中…失败”提示）。
2. `pending_creates` 只 `retain` 过期，不区分失败/成功；管理模式下新会话 command_id 复用不会去重。

建议：
- CommandAck 的 Rejected → 立即从 map 移除并 toast；
- map 加上限，超过即丢弃先进项。

---

### BUG-004 `Debian 12` 终端下 `/` 菜单高亮漂移 `[confirmed]`

位置：
- `src/app/composer_ops.rs:175`
- `src/ui/composer.rs:28`

要点：
1. menu 上下方显示的选中项与 `slash_selected` 可能漂移：一个用 `candidates.len()`，一个用实际 candidates 的 `take(6)` 截断。
2. `slash_selected` 的 clamp 只在 composer 字符级动作触发（Backspace、Delete、Char 输入），不覆盖 Alt/修饰键、命令解析过程中的候选收缩。

建议：
- 集中唯一的 clamp 入口（`clamp_slash_selected` 已有，需在 `slash_candidates` 收缩路径强制调用）。

---

### BUG-005 状态行情丢失：`ConnEvent` 状态覆盖连锁 `[confirmed]`

位置：
- `src/app/mod.rs:529`
- `src/runtime.rs:299`

要点：
1. `Ready` 事件每次 session_ctx 变化都刷新，但 `epoch_changed` 只比较 `known_epoch`；客户端重建后关闭 frame 可能乱序，`Ready` 事件的旧 epoch 视图会在 UI 中短暂反噬。
2. `StreamIssue` 仅写入 `conn_error`，不改变 `conn_phase`，UI 状态栏无法恢复为“Ready 但有告警”的表达，只能等待下一次 Ready。

建议：
1. `StreamIssue` 引入独立状态（`ReadyWithIssue`），UI 明确显示 reconnect 状态；
2. `Ready` 事件附带 `server_epoch` 校验，若 epoch 来自旧 client 直接 drop。

---

### BUG-006 面板 / A11y：未正确 abort 的 permission 面板可能挤压 Ask `[suspicious]`

位置：
- `src/app/mod.rs:999`
- `src/app/interaction.rs:141`

要点：
1. `pending_permissions` 是 Vec，允许同时多个 pending；`active_permission()` 永远取 first。当 tool 已经被 started/finished 清掉或 late-resolve 时出现“幽灵面板”，ask 下不来（优先级 permission > ask）。
2. `ToolPermissionRequested` 事件可能在 ToolStarted 之后“补投”，把已响应的 tool_call_id 重新加回来（路径上未见 filter）。

建议：
- `handle_tool` 在收到 Started/Finished 后，为对应 `tool_call_id` 加 `seen_responded` 集合；再次收到 Same-event 时直接跳过。

---

### BUG-007 子代理：失败被误标为 Closed `[confirmed]`

位置：
- `src/app/mod.rs:491`
- `src/app/subagent.rs:359`

要点：
- `TimelineLost` 对所有 seed 一旦命中 `subagent_seeds.contains` 就直接 `mark_subagent_closed`，导致失败/失联也显示 Closed；无法区分事实。

建议：
- 根据 error 中 status 判断 404（seed 已消失）才映射 Closed；其他错误保留原状态并提示。

---

### BUG-008 关闭父 tab 时，子代理 seed 与父不在同一 tabs 时不回收 `[confirmed]`

位置：
- `src/app/session_ops.rs:100`

要点：
1. 关闭父 tab（`close_tab_by_seed`）时，清理子代理路径未见有条件关闭非 tab 内子代理的回收逻辑，当父 seed 不在本地时无动作。
2. 后端主动关父时，子代理是否自动回收需要 daemon 确认，前端无兜底。

建议：
- 关闭父 tab 时遍历 `subagent_seeds`，按父级关系兜底回收。

---

### BUG-009 设置/配置：`ConfigChanged` 与 Draft 状态冲突 `[suspicious]`

位置：
- `src/app/mod.rs:714`
- `src/app/settings_ops.rs:18`

要点：
1. ConfigChanged 到来时 `profile_sel` 被置空，但如果用户正处于 Draft 编辑中，`save_settings` 只发 Draft 的脏字段；可能覆盖并发修改的部分字段。
2. `settings_saving` 在失败/成功后重置，但 ConfigChanged 重拉时可能使用旧 `draft`，造成显示层“已保存”但实际生效不完整。

建议：
- 在 ConfigWrite_into Draft 时根据后端 version/revision 加标签，避免并发覆盖。

---

### BUG-010 渲染缓存：`show_reasoning` 切换后与缓存的 old-width 冲突 `[confirmed]`

位置：
- `src/app/mod.rs:1363`（`F3` toggle 后统一 `s.rendered = None`）
- `src/ui/transcript.rs:25`

要点：
1. `F3` toggle 确实 invalidate；但 `ensure_render_caches` 立即用**当前宽度**重建，渲染时可能出现 temporarily 不一致/闪烁。
2. `timeline.version` 不会反映 `show_reasoning` 切换，当 `ensure` 与 `draw` 双线程时偶发旧 cache 界面。

建议：
- 增加原子性：要么在 handle 事件内同步重新 render，要么 add `show_reasoning` 到 `RenderedTranscript` 缓存 key。

---

---

## 未排查方向 / 需要后续确认

以下方向还没有做深度走查，不代表无 bug，仅说明不改代码前需优先确认：

1. **Session 补活/重连中的 `SessionResume` / `SessionAttach` 幂等性**
   - daemon 端对 repeated resume/attach 是否幂等、报错码是什么；
   - 失败路径是否会导致 app 状态卡死。

2. **状态码与 epoch 语义**
   - backend 对 epoch 版本的蓝图；
   - `ConnEvent::Ready epoch_changed` 的序列关系是否稳定。

3. **子代理 / subagent lifecycle**
   - `spawn_subagent` 重复 event、output 缺失、seed 复用的各路径；
   - `SubagentState::Closed` 的 daemon 端权威来源。

4. **保存路径 / 上传**
   - `upload_attachment` 的超时、失败、媒体 type 校验；
   - daemon 端对 ContentRef 的生命周期。

5. **插入 / 粘贴的双语言排版**
   - `wrap_text` 的 emoji / wide-char / 标点 backspace；
   - composer multi-line 与 history up 冲突。

6. **Markdown 遇到长 URL / code fence**
   - `render_markdown` 超宽度递归与性能；
   - 大代码块 / 表格截断的正确性。

7. **滚动 / 跟随逻辑**
   - `PageUp/Down` 与 `scroll.offset`、`timeline.cap_turns` 组合的边界。

8. **设置/配置 / config API**
   - `ConfigDto` / `ConfigPatch` 与后端 v-next 的 shape 漂移；
   - subagent_patch 字段映射。

9. **后台与工具**
   - `ToolEvent` / `TimelineToolProgress` 的 lossy 丢弃路径；
   - `apply_bash_progress` 在大量 chunk 下的内存上限。

10. **运行时生命周期**
    - `Runtime::rebuild` 失败后重试 + `generation` 订阅退出；
    - `watch_stall` 的边界（net silence vs truly dead）。

11. **测试基础设施**
    - repo 缺少 TUI 的端到端渲染测试；
    - overlay / modal / rewrite 快照测试缺失。

12. **DFS 遗留 / 编译类**
    - `large_enum_variant` 的实际内存压力；
    - 多路径 `Box<Timeline...>` 的后续迁移是否必要。

---

## 本次未覆盖的排查入口

以下相关代码路径还没有翻到细节，不建议在未复查前冻结结论：

- `qaqh-client` 内部 SSE 断线处理；
- `qaqh-runtime` 的 session resume / attach 权限映射；
- `ToolEvent::ToolPermissionRequested` 的 permission merge 顺序；
- `TimelineEvent::BlockCheckpoint` 在 double delivery 的覆盖规则；
- conn 中 daemon.json 重建鉴权 / fallback path。

