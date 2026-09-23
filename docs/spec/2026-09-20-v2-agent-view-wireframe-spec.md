# QAQH TUI v2 Agent View Wireframe

> 状态：**V2-M0 冻结候选**
> 日期：2026-09-20
> 上游计划：[`2026-09-20-v2视觉与交互重构-plan.md`](../plan/2026-09-20-v2视觉与交互重构-plan.md)
> 关联规范：
> [`2026-09-20-v2视觉token-spec.md`](2026-09-20-v2视觉token-spec.md) ·
> [`2026-09-20-v2终端提交协议-spec.md`](2026-09-20-v2终端提交协议-spec.md)

---

## 0. 目标

本规范定义 v2 默认 Agent View 的屏幕骨架、层级、组件顺序与窄屏降级。
它不是像素级美术稿，而是实现与快照测试的权威布局口径。

---

## 1. 设计约束

- 默认单列，无常驻 tab bar、无常驻 sidebar。
- 已提交历史在终端 scrollback；本图只表示视觉顺序。
- inline viewport 承载 live block、composer、activity、status、shortcuts。
- 不重边框；块身份用 accent rail + padding。
- 正文优先；chrome 在窄屏先降级。
- CJK 宽度按 `unicode-width` 计算。

---

## 2. 宽屏 Agent View（120×40）

```text
  ~/Projects/qaqh-tui-app                                  9.5K / 200K

  ❯ explain the v2 rendering plan
    ──────────────────────────────────────────────── 14:32

  ◆ QAQH v2 Render Plan

    The v2 default is terminal-native:

    • committed history goes to terminal scrollback
    • live content stays in an inline viewport
    • themes come from semantic tokens

    ────────────────────────────────────────────────

  ◇ Thought for 2.4s

  ⚙ read  docs/plan/2026-09-20-v2视觉与交互重构-plan.md
    ✓ 1.2s · 18 KB

  ┃ ⚙ 4 tool calls · 3✓ 1✗ · 2.1s · F7 展开
  ┃ ✗ exec  cargo test --all-targets
  ┃ │ error: one test failed
  ┃ │ …折叠 12 行…
  ┃ │ test result: FAILED
  ┃

  ◆ 结论

    视觉重构应按 V2-M0 → V2-M7 分阶段落地。
    第一阶段只做 token 与 inline 原型，不碰协议。

  ◇ Thinking…
  ┃ analyzing compatibility matrix

  ╭──────────────────────────────────────────────────────────╮
  │ ❯ Ask anything…                                          │
  │                                                          │
  ╰─ QAQH Night · build · qaqh-tui-app ──────────────────────╯

  ● ready · 12:34      ↑12k ↓8k · 42% · running         14:32
  Enter send · Ctrl+P mode · Ctrl+A attach · F1 help
```

要点：

- 顶部只有路径与 usage，不做 tab strip。
- 用户 prompt 使用填充色带 + 右对齐时间。
- assistant 使用 `◆` + accent rail，正文是 markdown。
- thinking 使用 `◇`，默认一行；运行中可显示最新行。
- 工具卡使用 `⚙` + accent rail；组折叠一行；失败内联。
- composer 是圆角框，底部内联 model / mode / cwd。
- status 与 shortcuts 各一行，动态变化。

---

## 3. 标准 Agent View（80×24）

```text
  ~/Projects/qaqh-tui-app                          9.5K / 200K

  ❯ explain v2
    ────────────────────────────────── 14:32

  ◆ V2 是 terminal-native 重构。

    已提交历史交给 scrollback，
    可变内容留在 inline viewport。

  ◇ Thought for 1.2s

  ⚙ read  plan.md
    ✓ 0.8s

  ◇ Thinking…
  ┃ checking compatibility

  ╭────────────────────────────────────────────╮
  │ ❯                                          │
  ╰─ QAQH Night · build ───────────────────────╯

  ● ready · 42% · running                14:32
  Enter send · F1 help
```

降级规则：

- 隐藏顶部 token 右侧之外的次要信息；
- 工具 metrics 只在宽屏显示；
- status 只保留连接、usage、activity、clock；
- shortcuts 只保留当前上下文最关键的 2-3 项。

---

## 4. 窄屏 Agent View（40×20）

```text
  ~/qaqh-tui-app                 9.5K/200K

  ❯ explain v2
    ────────────────── 14:32

  ◆ V2 是 terminal-native
    重构。

    已提交历史交给
    scrollback。

  ◇ 1.2s

  ⚙ read plan.md
    ✓

  ◇ Thinking…

  ╭──────────────────────────╮
  │ ❯                        │
  ╰─ QAQH Night ─────────────╯

  ● ready · running    14:32
```

窄屏纪律：

- 隐藏 shortcuts bar；
- status 压缩为连接 + activity + clock；
- 工具 metrics、耗时、bytes 可隐藏；
- 保留正文、accent rail、composer、失败信息；
- 不使用横向滚动。

---

## 5. Workspace View（会话选择器）

```text
  QAQH Workspace

  Sessions
  ❯ ● qaqh-tui-app        v2 visual reset        12:34
    ● qaqh-backend        v2 P0 gate             12:30
    ○ docs handoff        archived               09:12

  Actions
    n  new session
    r  rename
    a  archive
    d  delete
    Esc back

  Enter open · ↑↓ move · / search
```

- Workspace View 使用 alternate screen。
- 退出后回到 Agent View；scrollback 不受污染。
- 会话切换按终端提交协议 §7 重放。

---

## 6. Modal View

### 6.1 权限确认

```text
  ╭─ Permission Required ─────────────────────────╮
  │ exec wants to run:                            │
  │   cargo test --all-targets                    │
  │                                               │
  │ risk: medium                                  │
  │ cwd: ~/Projects/qaqh-tui-app                  │
  │                                               │
  │ a approve · d deny · t trust dir · Esc back   │
  ╰───────────────────────────────────────────────╯
```

### 6.2 ask_user

```text
  ╭─ Question ────────────────────────────────────╮
  │ Which renderer should v2 use first?           │
  │                                               │
  │ ❯ 1. inline viewport                          │
  │   2. fullscreen fallback                      │
  │   3. decide after prototype                   │
  │                                               │
  │ 1-9 select · e custom · Esc skip              │
  ╰───────────────────────────────────────────────╯
```

### 6.3 plan review

```text
  ╭─ Plan Review ─────────────────────────────────╮
  │ V2-M1 inline prototype                        │
  │                                               │
  │ 1. add Viewport::Inline shell                 │
  │ 2. add commit ledger                          │
  │ 3. add resize harness                         │
  │                                               │
  │ a approve · g approve+auto · r reject         │
  ╰───────────────────────────────────────────────╯
```

Modal 纪律：

- 复杂 modal 走 alternate screen；
- 快速确认可走 bottom panel；
- 关闭后恢复 Agent View；
- 不把 modal 内容写入 scrollback，除非用户确认结果摘要。

---

## 7. 组件顺序

Agent View 的 inline viewport 从上到下：

1. live assistant / tool / thinking
2. composer
3. activity line
4. status line
5. shortcuts line

scrollback 的顺序：

1. turn divider
2. user prompt
3. assistant message
4. thinking line
5. tool blocks / group
6. system event
7. turn footer / metrics

---

## 8. 键盘优先

| 键 | 行为 |
|---|---|
| Enter | 发送 |
| Shift+Enter | 换行 |
| Ctrl+P | mode |
| Ctrl+A | attach |
| Ctrl+L | workspace / session list |
| Ctrl+T | thinking replay |
| F1 | help |
| PgUp/PgDn | 终端原生 scrollback |
| Ctrl+C | cancel / quit armed |

鼠标是增强，不阻塞键盘流程。

---

## 9. 快照验收

- 120×40、80×24、40×20 三档 Agent View 快照。
- 用户 prompt、assistant、thinking、tool、group、failure、composer、status 各至少一张。
- Workspace View、permission、ask、plan 各至少一张。
- 主题矩阵：QAQH Night / Day / Terminal / NO_COLOR。
- CJK 宽字符后继 cell 占位正确。
