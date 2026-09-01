# Markdown 渲染 + 表格/高亮 设计（首版）

> 目标：首版 100MB RSS 硬上限 + 流式近无限制；性能兜底为批处理，后续再加速。
> 现状：`render_transcript.rs:95` 纯 `wrap_text`，`render_line.rs:9` 13 枚举色，`transcript.rs:32` 单 `Paragraph` 滚动，`ACTIVE_MODELS=4`/`TURNS_CAP=400` 已控主存。

## 1. 接口
```
src/app/markdown.rs
  pub fn render_markdown(text: &str, width: usize) -> Vec<RenderLine>
  pub fn is_markdown(text: &str) -> bool // 轻量探测：含 #*`_[]|:- 触发
  // 内部：pulldown_cmark::Parser::new_ext(text, OPTIONS) -> Vec<Event>
  //       状态机：heading/emphasis/inline_code/link/list/blockquote/rule
  //       表格：collect Table(alignments) -> rows:Vec<Vec<String>> -> 栅格 RenderLine
```

## 2. 依赖
* `pulldown-cmark = "0.12"` `Options::ENABLE_TABLES|ENABLE_STRIKETHROUGH|ENABLE_TASKLISTS`（逐个 insert，因无 GFM 聚合）
* 首版不引 `syntect`，二阶再加 `syntect 5` 单例 `SyntaxSet::load_defaults_newlines` + `ThemeSet::load_defaults` + `HighlightLines`

## 3. Span 扩展
* `render_line.rs:9` 增 `MarkdownH1/H2/H3/Bold/Italic/InlineCode/CodeBlock/Link/Quote/Ruler/TableHead/TableCell`
* 或增 `Custom(Style)` 以承载 `syntect Color::Rgb`；`theme.rs:7` 映射新增色，保持 `Paragraph` 单路径

## 4. 渲染分流
```
push_text_block(text, width, streaming):
  if streaming || !is_markdown(text) { wrap_text } // 流式禁 markdown，低 CPU
  else { render_markdown } // BlockCheckpoint/TurnCompleted 后单块富化
```
* 流式 100ms/16 条 `TimelineEntry` 批合并（`handle_runtime` 100ms 窗），`cap_turns` 前置

## 5. 表格栅格
* `Tag::Table(alignments)` 累积 `rows`，`End(Table)` 时按 `width` 等分列宽（CJK `width()`），溢出 `wrap_text` 列内换行或 `…`，`Alignment` 决定 pad，`┌─┬┐` 边框，头行 `Accent+BOLD`

## 6. 内存/性能指标
* 唯一物化 `Vec<RenderLine>`（width+version 键控），AST 不存；`PS/TS` 单例
* 基准：空/1k行/400回合 `heaptrack` 断言 `RSS<100MB`，流式 10k 行不爆；超阈自动降为 `TurnCompleted` 才富化

## 7. 验收
* `cargo test` 新增 `markdown` 单测：标题/列表/表/围栏；`clippy 0 error`
* 手工：`F10` 设置页无关，长文表格/代码块无闪烁，滚动条 `line_count`
