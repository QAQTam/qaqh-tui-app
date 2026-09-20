# V2-M5 Workspace / Modal 交付报告

> 日期：2026-09-20
> 状态：完成
> 基线：`betav2 @ efa05d9` 起

## 1. 交付内容

- 新增统一 `ScreenRoute` / `WorkspaceRoute` / `ModalRoute`。
- `--v2-agent` 实现 inline ↔ alternate-screen 生命周期：
  - Agent View：inline viewport + scrollback；
  - Workspace/Modal：alternate screen；
  - 返回时重建 inline viewport，不重复提交已封口块。
- Workspace：
  - Sessions：`Ctrl+L`、`/sessions`
  - Settings：`Ctrl+,`、`F10`、`/settings`
  - Help：`F1`、`/help`
  - Todo：`F4`、`/workspace`
  - Subagent：`Ctrl+↑` 进入，`Ctrl+↓` / `Esc` 返回
- Modal：
  - permission、ask_user、plan review；
  - Confirm、附件路径、cwd 输入、思考回放。
- 阻塞式 permission/ask/plan 期间，全局 Workspace 快捷键不再把 overlay 压到 Modal 下方。

## 2. 安全与正确性

- Workspace/Modal 期间禁止 `insert_before`，不会把管理面内容写进 scrollback。
- `Esc`/返回路径恢复 Agent View 后，ledger 继续保证历史幂等。
- permission 的拒绝/批准路径与 ask 的 1-based 选择保持不变。
- 退出/panic 继续由 `ratatui::restore()` 恢复终端。

## 3. 验证证据

```text
cargo fmt --check                            通过
cargo clippy --all-targets -- -D warnings    通过
cargo test --all-targets                     312 passed / 0 failed / 6 ignored
scripts/tests/ci-linux-parse-test.sh          15 passed / 0 failed
```

新增/覆盖：

- 路由优先级：permission > ask > plan > Workspace/Modal overlay。
- ScreenMode：Inline → Alternate、Workspace ↔ Modal 保持、Alternate → Inline。
- Sessions/Help/Todo Workspace 渲染。
- permission / plan / ask Modal 渲染。
- 阻塞 Modal 消费全局 Workspace 快捷键。
- 窄屏 CJK Workspace 渲染。

真实 daemon + PTY 冒烟（隔离 `QAQH_DATA_DIR`）：

- `Ctrl+L` → Sessions Workspace：观察到 `EnterAlternateScreen`。
- `Esc` → Agent View：观察到 `LeaveAlternateScreen` 与 inline 重绘。

## 4. 剩余风险

- permission/ask/plan 尚未做真实 daemon 故障注入与完整 PTY 交互矩阵；归入 V2-M6。
- Subagent Workspace 当前每帧重放 transcript，长会话性能归入 M6 基准与缓存优化。
