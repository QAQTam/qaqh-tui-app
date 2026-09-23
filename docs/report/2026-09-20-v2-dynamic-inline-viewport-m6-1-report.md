# QAQH TUI v2 M6.1 动态 Inline Viewport 报告

> 状态：**V2-M6.1 已完成**
> 日期：2026-09-20
> 关联计划：[`2026-09-20-v2视觉与交互重构-plan.md`](../plan/2026-09-20-v2视觉与交互重构-plan.md)
> 关联规范：
> [`2026-09-20-v2-agent-view-wireframe-spec.md`](../spec/2026-09-20-v2-agent-view-wireframe-spec.md) ·
> [`2026-09-20-v2终端提交协议-spec.md`](../spec/2026-09-20-v2终端提交协议-spec.md)

---

## 0. 结论

M6.1 移除了 Agent View 的固定 `Viewport::Inline(10)`。viewport 高度现在由当前
布局需求推导，并受终端高度与绝对上限约束：

- composer 按实际 CJK/ASCII 折行后的视觉行数参与计算；
- slash 菜单按当前候选数量参与计算；
- status 与 shortcuts 使用主题 token；
- 窄屏隐藏 shortcuts，并优先保留 composer 与 status；
- resize、composer 增长、slash 展开时按需重建 inline viewport；
- 重建只清理旧 viewport 区域，不重放 transcript，不触碰 commit ledger 和
  scrollback 中已提交内容。

---

## 1. 高度模型

### 1.1 组成

```text
inline viewport height
  = live tail rows
  + slash menu rows
  + composer visual rows
  + status rows
  + shortcuts rows
```

当前默认值：

| 项目 | 宽屏（>= 40 列） | 窄屏（< 40 列） |
|---|---:|---:|
| live tail | 4 | 2 |
| composer min | `theme.spacing.composer_min_height`（3） | 1 |
| composer max | `theme.spacing.composer_max_height`（8） | 8 |
| slash menu | 候选数，最多 4 | 候选数，最多 4 |
| status | 1 | 1 |
| shortcuts | 1 | 0 |

### 1.2 上限

```text
max_height = min(16, floor(terminal_height * 60%))
```

终端高度不足以容纳所有组件时，按以下顺序降级：

```text
live -> slash -> shortcuts -> composer -> status
```

`composer` 在任何可显示输入框的终端高度下至少保留 1 行；只有高度连
composer + status 都无法容纳时才牺牲 status。

---

## 2. 实现

### 2.1 布局事实源

`src/terminal/agent.rs` 新增 `AgentLayout`，渲染和 viewport 高度计算共用同一套
布局规则。`render_agent` 不再从固定高度反推可用空间，而是消费该布局结果。

### 2.2 viewport 重建

`TerminalHost` 记录：

- `inline_height`：当前实际 inline viewport 高度；
- `desired_inline_height`：当前布局目标高度。

每轮事件循环重新计算目标高度；只有 Agent View 处于 inline 模式且高度变化时才
重建：

1. 记录旧 viewport 的 `Rect`；
2. `Terminal::clear()` 清理旧 viewport 区域；
3. 将光标锚定到旧 viewport 顶部；
4. 使用新的 `Viewport::Inline(height)` 创建终端；
5. 下一帧完整重绘 live viewport。

`AgentState`、`V2TranscriptRuntime` 和 commit ledger 不参与重建，因此不会因为
resize 或高度变化产生重复提交。

### 2.3 alternate screen 与 pager

- Workspace/Modal 进入 alternate screen 时保留目标 inline 高度；
- 返回 Agent View 时按最新目标高度恢复；
- `$PAGER` 挂起/恢复时沿用同一高度记录；
- alternate screen 期间不调用 `insert_before`。

---

## 3. 测试

新增：

```text
terminal::agent::tests::dynamic_viewport_grows_for_composer_and_slash_menu
terminal::agent::tests::narrow_viewport_hides_shortcuts_and_preserves_composer_tail
terminal::agent::tests::viewport_height_respects_screen_ratio_and_cap
```

覆盖：

- 多行 composer 使 viewport 增长；
- `/` slash 菜单使 viewport 增长；
- 20 列窄屏 + CJK 输入时隐藏 shortcuts、保留 composer 尾部与 status；
- 1/2/4/8/24/40/80 行终端下不超过 60% 与 16 行上限；
- 既有 80×24 → 40×20 → 20×8 → 120×40 resize 测试继续通过。

---

## 4. 验证结果

```text
cargo fmt --check                            通过
cargo clippy --all-targets -- -D warnings    通过
cargo test --all-targets                     315 passed / 0 failed / 6 ignored
scripts/tests/ci-linux-parse-test.sh         15 passed / 0 failed
```

真实 daemon + PTY 冒烟（隔离 `QAQH_DATA_DIR`）：

- `--v2-agent --no-spawn` 成功进入 inline viewport；
- `Ctrl+N` 创建真实会话后，动态高度重建未导致进程崩溃或终端失步；
- 输入 `/` 触发 slash 布局路径后仍可继续运行；
- `Ctrl+Q` 退出时 bracketed paste、alternate screen 与光标恢复。

本切片未完成的硬化项：

- 使用终端模拟器断言重建前后的 scrollback 文本完全一致；
- 完整终端矩阵与高频 resize 故障注入；
- permission/ask/plan 的真实 PTY 交互矩阵。

这些留到 V2-M6.2/M6.3。

---

## 5. 下一步

**V2-M6.2：历史分页与 scrollback 插入顺序**

- 定义 `load_older`、re-baseline、会话切换时的插入顺序；
- 保证旧回合不重复、不倒退、不污染 live viewport；
- 把 commit ledger 的几何约束接入历史分页。
