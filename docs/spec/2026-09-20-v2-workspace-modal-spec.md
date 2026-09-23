# V2 Workspace / Modal 路由与生命周期规范

> 状态：M5 实现基线（2026-09-20）
> 关联计划：`docs/plan/2026-09-20-v2视觉与交互重构-plan.md`

## 1. 目标

- Agent View 始终是 inline viewport，已封口历史只通过 `insert_before` 进入终端 scrollback。
- Workspace 与 Modal 使用 alternate screen；退出后回到原 inline 锚点，不重放已提交块。
- 管理面入口与快捷键共用同一路由事实源，避免 v1/v2 行为漂移。
- 阻塞式交互期间，权限/ask/plan 不被后台 Workspace 覆盖。

## 2. 路由优先级

`src/ui/v2/route.rs` 是唯一路由解析入口，优先级如下：

1. `permission`
2. `ask_user`
3. `plan_review`
4. 顶层 overlay：Sessions / Settings / Help / Confirm / AttachPath / CwdInput / Thinking
5. 子代理观测
6. todo Workspace（`show_workspace`）
7. Agent View

Workspace：

- Sessions：`Ctrl+L`、`/sessions`
- Settings：`Ctrl+,`、`F10`、`/settings`
- Help：`F1`、`/help`
- Todo：`F4`、`/workspace`
- Subagent：`Ctrl+↑` 进入，`Ctrl+↓` / `Esc` 返回父会话

Modal：

- Permission：批准 / 拒绝 / 信任目录
- Ask：单题分页、1-based 快捷键、自定义输入
- Plan：批准 / 批准+自主 / 拒绝理由
- Confirm、附件路径、cwd 输入、思考回放

## 3. 终端生命周期

`src/terminal/agent.rs` 维护 `ScreenMode::{Inline, Alternate}`：

```text
Agent(route)          Inline
Workspace/Modal(route) Alternate
```

切换纪律：

1. Inline → Alternate：先提交 pending scrollback，再 `EnterAlternateScreen`。
2. Alternate 内 Workspace ↔ Modal：不重复进出 alternate。
3. Alternate → Inline：先 `LeaveAlternateScreen`，重建 inline terminal，再提交待写块。
4. Alternate 存在期间禁止调用 `insert_before`，避免管理面内容进入 scrollback。
5. 退出或 panic 仍由 `ratatui::restore()` 兜底离开 alternate screen。

## 4. 阻塞式交互纪律

- permission/ask/plan 存在时，除退出键外，全局快捷键先交给 modal 路由。
- `Ctrl+L` / `Ctrl+,` / `F1` 等不会再把 overlay 压到 modal 下方。
- Modal 结束后重新解析路由；若回到 Agent View，则恢复 inline 与 composer 光标。

## 5. 测试基线

- 路由优先级与 Workspace/Modal 分类单测。
- ScreenMode 状态机单测：进入、保持、退出不重复切换。
- ask 1-based 选择与分页回归。
- permission / plan 的 TestBackend 渲染回归。
- Workspace 窄屏与 CJK 回归。
- `cargo fmt --check`、`cargo clippy --all-targets -- -D warnings`、`cargo test --all-targets`。

2026-09-20 已在隔离数据目录下完成一次真实 daemon + PTY 冒烟：
`Ctrl+L` 进入 Sessions Workspace（观察到一次 `EnterAlternateScreen`），`Esc`
返回 Agent View（观察到 `LeaveAlternateScreen` 并重建 inline viewport）。
permission/ask/plan 的全链路故障注入与完整终端矩阵仍留到 M6 e2e 门禁。
