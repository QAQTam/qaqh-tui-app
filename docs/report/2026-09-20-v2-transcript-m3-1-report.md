# QAQH TUI v2 Transcript 报告（V2-M3.1）

> 状态：**M3.1 已完成；M3.2 commit/inline 接线待续**
> 日期：2026-09-20
> 上游计划：[`2026-09-20-v2视觉与交互重构-plan.md`](../plan/2026-09-20-v2视觉与交互重构-plan.md)
> 关联规范：
> [`2026-09-20-v2-agent-view-wireframe-spec.md`](../spec/2026-09-20-v2-agent-view-wireframe-spec.md) ·
> [`2026-09-20-v2-v1-parity-matrix.md`](../spec/2026-09-20-v2-v1-parity-matrix.md)

---

## 0. 结论

V2 Transcript 的第一层已完成：

- 新增独立 `src/ui/v2/transcript.rs` view model 与渲染器；
- 覆盖 User / Assistant / Thinking / Tool / System 五类块；
- 新增 `src/ui/v2/adapter.rs`，把现有 `timeline_model` 投影到 V2 block；
- 颜色全部来自 `Theme` token，`src/ui/v2/` 无直接 `Color::*`；
- 渲染前清洗 ANSI / C0 / C1 控制字符，避免 scrollback 终端注入；
- 保留 CJK 宽字符、工具折叠、diff、失败内联和主题矩阵测试。

本层刻意不改造 V1 `render_transcript`，避免在 M3 中途同时承担两套渲染路径的风险。

---

## 1. 模型

```text
TranscriptBlock
  id / revision / state
  kind:
    User
    Assistant
    Thinking
    Tool
    System
```

生命周期：

```text
Live -> Sealed -> Committed
          \-> Discarded
```

约束：

- `Live` 可变，`Sealed` 后 `replace_text` 拒绝修改；
- `Discarded` 不进入渲染输出；
- `revision` 保留给后续渲染缓存与 commit 冲突检测；
- `ToolState` 覆盖 Prepared / Running / Success / Failed / Cancelled / Backgrounded。

---

## 2. 渲染口径

### User

- accent rail + `❯`；
- 正文 `text.primary`；
- CJK 按 `unicode-width` 折行。

### Assistant

- `◆` accent rail；
- 基础 markdown：
  - H1-H6 → `markdown.h1..h6`；
  - inline code → `markdown.code`；
  - quote / rule / list / task；
  - fenced code → `markdown.code`；
- live block 尾部追加 cursor glyph。

### Thinking

- `◇`；
- live 只显示 `Thinking…` + 最新一行；
- sealed 显示 `Thought for 1.2s`；
- body 颜色走 `text.muted`，不抢占正文。

### Tool

- `⚙ name summary`；
- 状态行：Prepared / Running / Success / Failed / Cancelled / Backgrounded；
- 失败原因内联；
- diff `+/-/@@` 使用 `diff.*` / `semantic.command`；
- 终态正文首 3 行 + `…折叠 n 行…` + 末 3 行；
- running 正文只保留尾窗。

### System

- `·` info / `✗` warning+error；
- 错误走 `accent.error`。

---

## 3. 安全与性能边界

- `sanitize_text()` 先剥离 ANSI/VT，再过滤控制字符；
- 所有动态文本进入渲染前均清洗，防止 commit 到 scrollback 时注入终端控制序列；
- 工具正文有界，避免长输出把 inline viewport 拉爆；
- 500 block 压力测试确认长 transcript 渲染有界；
- 复用现有 `WrapState` / `wrap_text`，不另造一套 CJK 折行算法。

---

## 4. 测试

新增测试覆盖：

```text
ui::v2::transcript::tests::*                 11
ui::v2::adapter::tests::*                     3
```

重点用例：

- `user_block_wraps_cjk_without_exceeding_width`
- `assistant_markdown_uses_theme_tokens`
- `live_thinking_shows_latest_line`
- `tool_failure_is_inline_and_fold_is_visible`
- `render_sanitizes_terminal_control_sequences`
- `discarded_blocks_are_not_rendered`
- `sealed_block_rejects_text_mutation`
- `all_themes_render_without_color_leaks`
- `long_transcript_render_is_bounded`
- `transcript_snapshot_is_stable`
- `maps_user_and_core_blocks_in_order`
- `maps_tool_terminal_state_and_failure`

---

## 5. 验证结果

```text
cargo fmt --check                            # 通过
cargo clippy --all-targets -- -D warnings    # 通过
cargo test --all-targets                     # 270 passed / 0 failed / 6 ignored
scripts/tests/ci-linux-parse-test.sh         # 15 passed / 0 failed
```

颜色纪律：

```bash
rg 'Color::' src/ui/v2
# 无输出
```

---

## 6. 下一步

V2-M3.2：inline / commit 接线。

- 用 `adapter::from_turn` 接 timeline；
- live block 只进 inline viewport；
- Sealed 后生成提交行并通过 `CommitLedger` 写入 scrollback；
- replay / resize / reconnect 保持幂等；
- 接入 `BlockCheckpoint` / `TurnSealed` 的提交时机；
- 完成后再进入 V2-M4 Composer / Status / Shortcuts。
