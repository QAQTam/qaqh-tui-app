# QAQH TUI v2 M6.1 动态 Inline Viewport 下班 handoff（2026-09-20）

## 交接摘要

M6.1 已完成并推送，当前是干净可继续开发的状态。下一班可直接从 M6.2
（历史分页与 scrollback 插入顺序）开始，不需要先修复本轮代码。

| 项 | 值 |
|---|---|
| 仓库 | `/home/qaqtamsy/项目/qaqh-tui-app` |
| 分支 | `betav2` |
| HEAD | `5804a5f feat(v2): add dynamic inline viewport` |
| 远端 | `origin/betav2` 同步 |
| PR | https://cnb.cool/QAQ-Harness/qaqh-tui-app/-/pulls/39 |
| 工作树 | 干净 |
| 后端仓库 | `/home/qaqtamsy/项目/qaqh-backend` |
| 后端 HEAD | `b80028b Merge PR #201: docs(v2): 回写 P2-1 合并证据` |
| 最近确认的 TUI 契约锚点 | `50d3dc1dcfef Merge PR #175: replay window status`（⚠ 已过期，2026-09-23 刷新为 `5ec1900d6c937b6`，见 [锚点刷新报告](../report/2026-09-23-backend-anchor-refresh-5ec1900-report.md)） |
| 遗留进程 | 无 `qaqh-daemon` / `qaqh-tui` |

---

## 1. 本轮完成

### 动态 inline viewport

- 移除 Agent View 的固定 `Viewport::Inline(10)`。
- 新增统一 `AgentLayout`，高度由以下内容共同推导：
  - live transcript tail
  - composer 实际 CJK/ASCII 折行行数
  - slash 菜单候选行数
  - status / shortcuts 主题 token
- 高度约束：
  - 终端高度的 60%
  - 绝对上限 16 行
- 窄屏（`<40` 列）：
  - 隐藏 shortcuts
  - 优先保留 composer、status
  - 保留 CJK 输入尾部窗口与光标可见性
- alternate screen / `$PAGER` 返回 Agent View 时复用最新 inline 高度。

### viewport 重建与提交安全

`TerminalHost` 现在维护：

- `inline_height`：当前实际 inline 高度；
- `desired_inline_height`：当前布局目标高度。

高度变化时：

1. 记录旧 viewport `Rect`；
2. 清理旧 viewport 区域；
3. 将光标锚定到旧 viewport 顶部；
4. 用新的 `Viewport::Inline(height)` 重建终端；
5. 下一帧完整重绘 live viewport。

重建不修改 `AgentState`、`V2TranscriptRuntime` 或 commit ledger，因此不会重放已封口
transcript，也不应重复写入 scrollback。

### 测试与文档

新增测试：

```text
terminal::agent::tests::dynamic_viewport_grows_for_composer_and_slash_menu
terminal::agent::tests::narrow_viewport_hides_shortcuts_and_preserves_composer_tail
terminal::agent::tests::viewport_height_respects_screen_ratio_and_cap
```

已更新：

- `src/terminal/agent.rs`
- `docs/plan/2026-09-20-v2视觉与交互重构-plan.md`
- `docs/report/2026-09-20-v2-agent-view-m4-report.md`
- `docs/report/2026-09-20-v2-dynamic-inline-viewport-m6-1-report.md`

本轮提交规模：4 文件，`+432 / -49`。

---

## 2. 验证结果

```text
cargo fmt --check                            通过
cargo clippy --all-targets -- -D warnings    通过
cargo test --all-targets                     315 passed / 0 failed / 6 ignored
scripts/tests/ci-linux-parse-test.sh         15 passed / 0 failed
```

真实 daemon + PTY 冒烟（隔离 `QAQH_DATA_DIR`）：

- `--v2-agent --no-spawn` 成功进入 inline viewport；
- `Ctrl+N` 创建真实会话，动态高度重建未崩溃；
- 输入 `/` 走 slash 布局路径后仍可运行；
- `Ctrl+Q` 退出后 bracketed paste、alternate screen、光标恢复。

---

## 3. 未完成与风险

### 本轮明确未验证

- 尚未用终端模拟器断言 resize/rebuild 前后 scrollback 文本逐字节一致；
- 尚未做高频 resize 故障注入；
- 尚未完成完整终端兼容矩阵；
- permission / ask / plan 的真实 daemon PTY 交互矩阵仍待 M6.3。

### 需要注意的实现点

- `TerminalHost::rebuild_inline` 通过重新创建 `Terminal::with_options` 改变 inline
  高度；这是当前 ratatui API 下无需改上游依赖的做法。
- 重建会触发一次终端 cursor position 查询，后续若在 tmux/SSH/特殊终端发现
  resize 延迟或 cursor query 竞争，应优先检查这里。
- 当前动态高度只由 composer、slash、status、shortcuts 推导；live transcript
  仍按 tail window 留在分配空间内，不因流式内容行数变化而持续改变 viewport 高度。
- 不要提前实现 P5 的 SessionModel、重连、driver capability；等 wire schema 与
  `qaqh-client` typed API 冻结后集中适配。

---

## 4. 下一班建议入口

### 首选：V2-M6.2 历史分页与 scrollback 插入顺序

目标：

- 定义 `load_older`、re-baseline、会话切换时的插入顺序；
- 保证旧回合不重复、不倒退、不污染 live viewport；
- 将 commit ledger 与历史分页的几何约束统一；
- 增加分页顺序、re-baseline、会话切换回归测试。

随后：

1. **M6.3**：真实 daemon + PTY 硬化，覆盖 permission / ask / plan、Workspace
   进出、resize、断线重连、多会话切换。
2. **M6.4**：feature flag、parity、长会话性能与内存基准。
3. 再考虑 **M7**：默认切换与 v1 滚动路径清理。

### 恢复上下文时建议先读

- `docs/report/2026-09-20-v2-dynamic-inline-viewport-m6-1-report.md`
- `docs/plan/2026-09-20-v2视觉与交互重构-plan.md`
- `docs/spec/2026-09-20-v2终端提交协议-spec.md`
- `docs/spec/2026-09-20-v2-agent-view-wireframe-spec.md`
- `src/terminal/agent.rs`

### 恢复开发命令

```bash
cd /home/qaqtamsy/项目/qaqh-tui-app
git status --short --branch
git log -3 --oneline

cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test --all-targets
```

若后续涉及 Rust/TUI 实现，重新加载适用 skills：`rust-router`、`domain-cli`，
必要时 `coding-guidelines`。

---

## 5. 协作约束

- 前端由主代理直接主导，不委派 A/B 工程师；
- 以本地后端为锚点，后端变化分批适配，不零散追赶；
- UI/交互设计与实现由主代理主导；
- P5 变更先了解接口范围，暂不提前落地；
- 当前没有需要清理的 daemon、TUI 或 PTY 进程。
