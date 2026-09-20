# QAQH TUI v2 视觉 Token 规范

> 状态：**V2-M0 冻结候选**
> 日期：2026-09-20
> 上游计划：[`2026-09-20-v2视觉与交互重构-plan.md`](../plan/2026-09-20-v2视觉与交互重构-plan.md)
> 适用范围：v2 Agent View、Workspace View、Modal View、markdown、工具块、composer、status。
> 原则：所有 v2 颜色、间距、边框、字形必须来自本规范；组件不得直接写 `Color::*`。

---

## 0. 一句话

v2 用**语义 token**取代 v1 的局部硬编码颜色，用**低 chrome、单列、accent rail**
取代重边框和仪表盘式视觉；默认主题是 QAQH Night，同时提供 QAQH Day 与 Terminal 原生主题。

---

## 1. Token 结构

建议实现为嵌套语义结构，避免 50+ 扁平字段散落调用点：

```rust
pub struct Theme {
    pub surface: SurfaceTokens,
    pub text: TextTokens,
    pub accent: AccentTokens,
    pub semantic: SemanticTokens,
    pub chrome: ChromeTokens,
    pub diff: DiffTokens,
    pub markdown: MarkdownTokens,
    pub glyph: GlyphTokens,
    pub spacing: SpacingTokens,
}

pub struct SurfaceTokens {
    pub base: Color,
    pub light: Color,
    pub dark: Color,
    pub highlight: Color,
    pub hover: Color,
}

pub struct TextTokens {
    pub primary: Color,
    pub secondary: Color,
    pub dim: Color,
    pub muted: Color,
    pub bright: Color,
}

pub struct AccentTokens {
    pub user: Color,
    pub assistant: Color,
    pub thinking: Color,
    pub tool: Color,
    pub system: Color,
    pub error: Color,
    pub success: Color,
    pub running: Color,
}

pub struct SemanticTokens {
    pub command: Color,
    pub path: Color,
    pub warning: Color,
    pub plan: Color,
    pub verify: Color,
}

pub struct ChromeTokens {
    pub border: Color,
    pub border_active: Color,
    pub selection: Color,
    pub scrollbar: Color,
}
```

实现约束：

- `Theme::current()` 是唯一取色入口。
- `ThemeKind` 至少包含 `QaqhNight` / `QaqhDay` / `Terminal` / `Auto`。
- 颜色写入前统一经过 `quantize(ColorSupport)`：
  `TrueColor -> Rgb`，`Ansi256 -> Indexed`，`Ansi16 -> named Color`。
- `NO_COLOR` 时所有前景色退化为 `Reset`，仅保留 bold / italic / underline / reverse。
- 组件只消费 token，不消费 palette 常量。

---

## 2. QAQH Night（默认）

QAQH Night 是近黑、低饱和、冷灰基底；强调色参考 Nord/TokyoNight 的可读性，但不复制
Grok 品牌色。

### 2.1 Surface

| Token | TrueColor | 256 fallback | 16 fallback | 用途 |
|---|---:|---:|---:|---|
| `surface.base` | `#0B0D10` | `Indexed(232)` | `Black` | 终端主画布 |
| `surface.dark` | `#08090B` | `Indexed(233)` | `Black` | 最深底、代码块 |
| `surface.light` | `#151922` | `Indexed(234)` | `Black` | 次级 surface |
| `surface.highlight` | `#1D2430` | `Indexed(235)` | `Black` | 选中/输入区 |
| `surface.hover` | `#252D3B` | `Indexed(236)` | `DarkGray` | hover / dropdown |

### 2.2 Text

| Token | TrueColor | 256 fallback | 16 fallback | 用途 |
|---|---:|---:|---:|---|
| `text.primary` | `#E6E9EF` | `Indexed(255)` | `White` | 正文 |
| `text.secondary` | `#B8BFCC` | `Indexed(250)` | `Gray` | 次级正文 |
| `text.dim` | `#5B6472` | `Indexed(240)` | `DarkGray` | 最弱元信息 |
| `text.muted` | `#7B8494` | `Indexed(244)` | `DarkGray` | 注释、折叠 |
| `text.bright` | `#9AA3B2` | `Indexed(247)` | `Gray` | 工具 accent |

### 2.3 Accent

| Token | TrueColor | 256 fallback | 16 fallback | 用途 |
|---|---:|---:|---:|---|
| `accent.user` | `#D8DEE9` | `Indexed(253)` | `White` | 用户 prompt rail |
| `accent.assistant` | `#B48EAD` | `Indexed(139)` | `Magenta` | assistant rail |
| `accent.thinking` | `#B48EAD` | `Indexed(139)` | `Magenta` | thinking |
| `accent.tool` | `#8FBCBB` | `Indexed(109)` | `Cyan` | tool |
| `accent.system` | `#81A1C1` | `Indexed(110)` | `Blue` | system |
| `accent.error` | `#BF616A` | `Indexed(131)` | `Red` | error |
| `accent.success` | `#A3BE8C` | `Indexed(108)` | `Green` | success |
| `accent.running` | `#88C0D0` | `Indexed(110)` | `Cyan` | running |

### 2.4 Semantic

| Token | TrueColor | 256 fallback | 16 fallback | 用途 |
|---|---:|---:|---:|---|
| `semantic.command` | `#EBCB8B` | `Indexed(180)` | `Yellow` | shell command |
| `semantic.path` | `#D08770` | `Indexed(173)` | `Yellow` | path |
| `semantic.warning` | `#EBCB8B` | `Indexed(180)` | `Yellow` | warning |
| `semantic.plan` | `#E5C07B` | `Indexed(180)` | `Yellow` | plan mode |
| `semantic.verify` | `#B48EAD` | `Indexed(139)` | `Magenta` | verify |

### 2.5 Chrome

| Token | TrueColor | 256 fallback | 16 fallback | 用途 |
|---|---:|---:|---:|---|
| `chrome.border` | `#2E3440` | `Indexed(236)` | `DarkGray` | 默认边框 |
| `chrome.border_active` | `#4C566A` | `Indexed(240)` | `Gray` | 聚焦边框 |
| `chrome.selection` | `#3B4252` | `Indexed(237)` | `DarkGray` | 选中背景 |
| `chrome.scrollbar` | `#434C5E` | `Indexed(238)` | `DarkGray` | 滚动条 |

### 2.6 Diff

| Token | TrueColor | 256 fallback | 16 fallback |
|---|---:|---:|---:|
| `diff.add_fg` | `#A3BE8C` | `Indexed(108)` | `Green` |
| `diff.add_bg` | `#1F2A1F` | `Indexed(22)` | `Green` |
| `diff.del_fg` | `#BF616A` | `Indexed(131)` | `Red` |
| `diff.del_bg` | `#2E1B1E` | `Indexed(52)` | `Red` |
| `diff.equal_fg` | `#7B8494` | `Indexed(244)` | `DarkGray` |
| `diff.gutter_fg` | `#5B6472` | `Indexed(240)` | `DarkGray` |

### 2.7 Markdown

| Token | TrueColor | 用途 |
|---|---:|---|
| `md.h1` | `#8FBCBB` | H1 |
| `md.h2` | `#81A1C1` | H2 |
| `md.h3` | `#B48EAD` | H3 |
| `md.h4` | `#9AA3B2` | H4 |
| `md.h5` | `#7B8494` | H5 |
| `md.h6` | `#5B6472` | H6 |
| `md.text` | `#B8BFCC` | 正文 |
| `md.code` | `#88C0D0` | inline code |
| `md.code_bg` | `#14181F` | code block surface |
| `md.link` | `#81A1C1` | link |
| `md.quote` | `#7B8494` | quote |
| `md.rule` | `#4C566A` | rule |
| `md.table_head` | `#8FBCBB` | table head |
| `md.task_done` | `#A3BE8C` | task checked |
| `md.task_todo` | `#B8BFCC` | task unchecked |

---

## 3. QAQH Day

QAQH Day 覆盖同一 token 集，供亮色终端与 `Auto` 主题使用。

| Token | TrueColor |
|---|---:|
| `surface.base` | `#F7F8FA` |
| `surface.dark` | `#EEF1F5` |
| `surface.light` | `#FFFFFF` |
| `surface.highlight` | `#E6EAF0` |
| `surface.hover` | `#DDE3EB` |
| `text.primary` | `#1F2937` |
| `text.secondary` | `#4B5563` |
| `text.dim` | `#9CA3AF` |
| `text.muted` | `#6B7280` |
| `text.bright` | `#4B5563` |
| `accent.user` | `#111827` |
| `accent.assistant` | `#7C3AED` |
| `accent.thinking` | `#7C3AED` |
| `accent.tool` | `#0F766E` |
| `accent.system` | `#2563EB` |
| `accent.error` | `#DC2626` |
| `accent.success` | `#16A34A` |
| `accent.running` | `#0891B2` |
| `semantic.command` | `#B45309` |
| `semantic.path` | `#C2410C` |
| `semantic.warning` | `#B45309` |
| `semantic.plan` | `#A16207` |
| `semantic.verify` | `#7C3AED` |
| `chrome.border` | `#D1D5DB` |
| `chrome.border_active` | `#9CA3AF` |
| `chrome.selection` | `#DBEAFE` |
| `diff.add_fg` | `#15803D` |
| `diff.add_bg` | `#DCFCE7` |
| `diff.del_fg` | `#B91C1C` |
| `diff.del_bg` | `#FEE2E2` |
| `md.h1` | `#0F766E` |
| `md.h2` | `#2563EB` |
| `md.h3` | `#7C3AED` |
| `md.code` | `#0F766E` |
| `md.code_bg` | `#F1F5F9` |
| `md.link` | `#2563EB` |

---

## 4. Terminal 原生主题

`Terminal` 主题不画 surface：

- `surface.* = Reset`；
- `text.primary = Reset`，`text.secondary = Reset + dim`；
- `accent.*` 使用 ANSI 16 色命名；
- 选中行使用 `REVERSED`；
- 边框使用 bright black；
- 聚焦 composer 边框使用默认前景；
- 不修改终端光标颜色。

---

## 5. 间距与布局 Token

| Token | 值 | 用途 |
|---|---:|---|
| `space.0` | 0 | 无间距 |
| `space.1` | 1 | rail 内边距、紧凑行 |
| `space.2` | 2 | 块左右 padding、段落 |
| `space.3` | 3 | 块间 gap |
| `space.4` | 4 | section gap |
| `rail.width` | 1 | accent rail |
| `block.pad_left` | 2 | 内容左 padding |
| `block.pad_right` | 2 | 内容右 padding |
| `outer.pad` | 2 | Agent View 外层 padding |
| `composer.min_height` | 3 | 输入框最小高度 |
| `composer.max_height` | 8 | 输入框最大高度 |
| `status.height` | 1 | 状态行 |
| `shortcuts.height` | 1 | 快捷键行 |

布局纪律：

- 内容宽度 = 终端宽度 − `outer.pad * 2` − `rail.width` −
  `block.pad_left` − `block.pad_right`。
- 窄终端优先保正文，再隐藏 shortcuts、status 次要字段、rail。
- 不引入固定侧栏；workspace 只在 Workspace View 出现。

---

## 6. 边框与字形

### 6.1 边框

| Token | 值 | 用途 |
|---|---|---|
| `border.none` | 无 | 默认 transcript 块 |
| `border.hairline` | `│` | rail / composer 内部 |
| `border.rounded` | `╭─╮ │ ╰─╯` | composer、modal |
| `border.heavy` | `┃` | 仅 v1 兼容，不在 v2 默认使用 |

### 6.2 Glyph

| Token | 值 | 用途 |
|---|---|---|
| `glyph.user` | `❯` | 用户 prompt |
| `glyph.assistant` | `◆` | assistant 块 |
| `glyph.thinking` | `◇` | thinking |
| `glyph.tool` | `⚙` | tool |
| `glyph.success` | `✓` | 成功 |
| `glyph.failure` | `✗` | 失败 |
| `glyph.running` | `◐` | 运行中 |
| `glyph.system` | `·` | system |
| `glyph.quote` | `▎` | quote |
| `glyph.fold` | `…` | 折叠 |
| `glyph.truncated` | `◌` | 截断 |

---

## 7. 组件到 Token 的映射

| 组件 | 主要 token |
|---|---|
| `UserPrompt` | `accent.user` + `surface.highlight` + `text.primary` |
| `AssistantMessage` | `accent.assistant` + `md.*` |
| `ThinkingLine` | `accent.thinking` + `text.muted` |
| `ToolBlock` | `accent.tool` + `text.secondary` + `semantic.command/path` |
| `ToolGroup` | `accent.tool` + `text.muted` |
| `TurnDivider` | `chrome.border` + `text.dim` |
| `Composer` | `surface.highlight` + `chrome.border_active` + `accent.user` |
| `StatusLine` | `text.dim` + `accent.success/error/running` |
| `ShortcutsBar` | `text.muted` + `text.secondary` |
| `Modal` | `surface.light` + `chrome.border_active` + `semantic.warning` |
| `WorkspacePane` | `surface.base` + `chrome.border` + `accent.system` |

---

## 8. 可访问性与降级

- 禁止只靠颜色表达状态；必须同时有 glyph 或文字。
- `NO_COLOR=1`：不输出颜色，只保留修饰符与 glyph。
- 16 色：使用 fallback 列，不丢语义。
- 256 色：优先使用 `Indexed` fallback，不模拟 truecolor。
- 亮/暗主题都需通过对比度人工检查。
- CJK、emoji、宽字符不得被 accent rail 或边框切裂。

---

## 9. 验收

- v2 组件目录内 `rg 'Color::'` 零命中（`theme/` 自身除外）。
- `Theme::current()` 是唯一主题入口。
- 三套主题 + Auto 均有快照。
- truecolor / 256 / 16 / NO_COLOR 四档均有测试。
- 组件快照覆盖 UserPrompt、AssistantMessage、ThinkingLine、ToolBlock、Composer、Status、Modal。
