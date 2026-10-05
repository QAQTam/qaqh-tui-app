# UI 设计对比分析：qaqh × Grok CLI × Codex CLI

> 分析对象：`E:/qaqh-tui-app`（本仓库）、`E:/grok-build-main`（Grok CLI，Rust TUI，核心 crate `xai-grok-pager`）、`E:/myXCode`（OpenAI Codex CLI，`codex-rs/tui`）。
> 结论已落地为代码改造（见文末「落地清单」），分支 `feature/ui-card-rework`。

## 一、三家交互形态总览

| 场景 | qaqh（改造前） | Codex | Grok |
|---|---|---|---|
| 工具授权 | **整屏清空 + 居中弹窗**，对话不可见 | 底部内联面板，替换 composer，对话可见 | 底部 dock 卡片，替换 composer 槽位，对话可见 |
| 向用户提问 | 整屏清空 + 居中弹窗 | 底部内联面板，多题 Tab 切换/notes | 底部卡片，多题 Tab，sticky 自定义输入 |
| 设置 | **整屏 Workspace**，只能跟随焦点滚 | 拆成多个底部 picker（/model /theme…） | 居中模态（70% 宽），分组单列表 + 过滤 |
| 布局哲学 | 三种全屏 ScreenRoute 互斥 | 内联视口：历史进原生 scrollback，ratatui 只画底部 | 双屏模式（全屏 alt-screen + minimal inline） |

三家里只有我们把阻塞交互做成「清屏只画弹窗」。Codex 与 Grok 的共识是：**阻塞卡片占一小块，对话上下文全程可见**。

## 二、值得吸收的设计（已落地或建议）

### 已落地 ✅

1. **卡片悬浮而非全屏（Codex Inline / Grok Card-in-dock）**
   - 改造前 Modal 与 Settings 都是整屏替换；现在 permission/ask/plan/confirm/思考回放与设置全部改为「agent 视图做背景 + 整屏 DIM 压暗 + 圆角卡片居中」。用户授权时能看到对话里模型到底在干什么——这是三家里我们此前最大的体验差距。

2. **内容自适应高度（Codex 审批卡的 desired_height 思路）**
   - 改造前 ask 弹窗硬顶 30 行、permission 硬顶 22 行——大终端上明明放得下也盲滚。现在高度 = min(内容需要, 终端可用)，permission 按内容估算行数（`permission_content_height`），16 条路径在 100x40 终端上全部可见。

3. **溢出必须可见（Codex 的 `[… N lines] ⌃g view all` / Grok 的滚动条）**
   - ask 此前是唯一没有溢出提示的阻塞弹窗。现在统一：顶边框右缘 `▼N` 徽标（还剩 N 行）、内容右缘 `▲`/`▼` 边缘指示、permission footer 保留「⚠ 还有 N 行未显示」警示。ask 另补了滚动 clamp（旧实现 PgDn 多按会滚出一屏空白）。

4. **选项折行而非截断（Codex `SelectionRowDisplay::Wrapped`）**
   - ask 的选项与自定义输入此前 `truncate_width` 加 `…`，长命令/路径选项永远看不全。现改为 CJK 感知折行（`wrap_text`），续行缩进对齐；渲染与命中共用同一份行向量，不会错位。Codex 证明折行 + 共享几何是正解，Grok 注释里自己承认高度核算与渲染会 drift。

5. **滚轮滚动卡片内容（Grok blocking card 语义）**
   - Modal 路由下滚轮此前是 no-op；现在直接滚卡片内容（permission/ask/plan），背景 transcript 不动。

6. **设置自由滚动（Grok settings modal 单列滚动）**
   - 设置卡片支持 PgUp/PgDn 与滚轮自由滚视口；焦点移动时自动跟随焦点行。旧实现只能「焦点顶到哪滚到哪」，顶部内容永远看不见。

7. **统一卡片 chrome（Grok modal_window / Codex surface）**
   - 圆角边框 + 语义色边框（授权=警示色、提问=激活色、计划=plan 色、思考=thinking 色）+ 未保存标记（`⚙ 设置 · ● 未保存`）。

### 值得后续吸收（未在本次落地）

- **Codex「决策即历史」**：批准/拒绝以 history cell 写进 transcript，可回溯可审计。我们的授权结果只进状态。
- **Codex 打字保护延迟（1s idle）**：审批弹窗等用户停止输入再出现，多任务时不打断输入流。
- **Codex Esc 分层（EscStep 即数据）**：Grok 的 Esc 级联每一 rung 有标签、快捷键提示条读的就是它——「提示永不撒谎」。我们的 Esc 语义散在各 key 分支里。
- **Grok 权限卡作用域收窄（←/→ 收缩 always-allow 的命令前缀，含持久化可行性校验）**：细粒度授权的产品级标杆。
- **Grok 主题工程纪律**：60+ 语义槽、启动量化管线、「颜色全部出自 Theme 结构体」强制约束。我们已有 token 体系，可补一条 lint 级约束。

## 三、明确不抄的设计

- **Codex 内联视口整套基建**（custom_terminal、scrollback reflow、PreWrap/Terminal 双策略）：为了把历史写进终端原生 scrollback 付出巨大复杂度税，且与我们的 alternate-screen fullscreen shell 架构冲突。Grok 也只把 minimal 模式作为实验特性。
- **Codex 单文件巨石**：chat_composer 12.9k 行、request_user_input 3.9k 行，SelectionItem 承载 20+ 字段。我们按模块拆分（modal.rs/workspace.rs/settings.rs）的路子是对的。
- **Grok 的高键位密度**（a-f 直选与 hjkl 挤在同一命名空间、Ctrl+F/O/C/E/.）：提示条兜底也救不了学习曲线。我们保留 1-9/a-f 直选 + Esc 语义不变。
- **Grok 的高度缓存增量失效**（gaps_may_be_dirty、Case 1/2/3）：注释自认脆弱。我们的「渲染与命中共用一份行向量」纪律更简单也更稳。

## 四、为什么「背景不登记命中」是安全的

strict probe 要求每个登记区域的锚点在真实 buffer 上非空。卡片用 `Clear` 覆盖背景后，背景锚点会变成空白 cell 而报 `anchor_missing`。因此背景绘制用 `HitMapBuilder::suspended()` 挂起登记（`hit.rs`），命中全部落在卡片自身；点卡片外由既有 `ModalRoot` 阻断层兜底，语义不变。整帧组合（背景+暗化+卡片）已加回归测试 `permission_modal_renders_over_agent_background` 过 strict probe。

## 五、落地清单（代码）

- `src/terminal/agent/mod.rs`：`draw()` 三分支改造（Modal/Settings 卡片走背景+DIM）；`dim_screen` 压暗层；滚轮滚动卡片（`modal_wheel_scroll_up` + `App::modal_wheel_scroll`）。
- `src/ui/v2/modal.rs`：全部弹窗换卡片 chrome；尺寸按内容自适应（`*_WIDTH` 常量 + 高度收口）；ask 选项/自定义折行、滚动 clamp、`▼N` 徽标、右缘 `▲▼` 指示；plan 正文+Todo 合一行向量统一滚动（修掉 Todo 段被静默裁掉的旧账）；confirm 正文折行。
- `src/ui/v2/workspace.rs`：新增 `draw_settings_card`（居中卡片 + 自由滚动 + 溢出徽标 + 编辑光标定位），删除旧全屏 `draw_settings`。
- `src/ui/v2/hit.rs`：`HitMapBuilder::suspended()/set_suspended()/is_suspended()`。
- `src/app/settings.rs`：`SettingsState.scroll` 自由滚动偏移；`move_focus` 归零滚动；`total_lines()` 行数估算。
- `src/app/overlay_ops.rs` / `interaction.rs`：设置 PgUp/PgDn 与滚轮接自由滚动；`modal_wheel_scroll`。
- `src/terminal/agent/fullscreen.rs`：背景模式不抢硬件光标（光标属于前景卡片）。

测试：454 通过（含 6 个新增：ask clamp、permission 大终端自适应、设置卡片 probe + 溢出徽标、整帧背景+卡片 probe、滚轮滚卡片）。
