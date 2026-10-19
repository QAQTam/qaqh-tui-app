//! 代码块渲染与 syntect 高亮。
//!
//! syntect 只负责回答「这段属于哪个语法 scope」；颜色一律经 base16-ocean 基色
//! 就近取回主题 token（见 `syntect_foreground`），不直接搬运 syntect 的 RGB——
//! 否则会在 16 色/`NO_COLOR` 终端上漏出真彩色，并在亮色主题上铺一层为暗底设计
//! 的浅灰前景。

use std::sync::OnceLock;

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use syntect::easy::HighlightLines;
use syntect::highlighting::{FontStyle, ThemeSet};
use syntect::parsing::{SyntaxReference, SyntaxSet};
use unicode_width::UnicodeWidthStr;

use super::CODE_INDENT;
use super::fg;
use super::text::{StyledSpan, wrap_styled};
use crate::theme::Theme;

/// 代码块在渲染期间累积的原始文本与语言。
pub(super) struct CodeState {
    pub(super) lang: Option<String>,
    pub(super) text: String,
}

static SYNTAX_SET: OnceLock<SyntaxSet> = OnceLock::new();
static THEME_SET: OnceLock<ThemeSet> = OnceLock::new();

fn syntax_set() -> &'static SyntaxSet {
    SYNTAX_SET.get_or_init(SyntaxSet::load_defaults_newlines)
}

fn theme_set() -> &'static ThemeSet {
    THEME_SET.get_or_init(ThemeSet::load_defaults)
}

pub(super) fn render_code_block(
    text: &str,
    lang: Option<&str>,
    width: usize,
    theme: &Theme,
) -> Vec<Line<'static>> {
    // 生成式 UI 围栏：后端 prompt 默认就会发它（`qaqh-ui v=0.2 format=tsx|jsonl`）。
    // 本端不渲染 DSL，但**也不能把原文倒给用户**——那是模板语法，不是内容。
    if let Some(info) = generative_ui_info(lang) {
        return render_generative_ui_placeholder(text, &info, width, theme);
    }
    let content_width = width.saturating_sub(CODE_INDENT.width()).max(1);
    let syntax_set = syntax_set();
    let syntax_theme = &theme_set().themes["base16-ocean.dark"];
    // **一个代码块共用一台 highlighter**：syntect 的解析状态（块注释、三引号
    // 字符串、模板字符串、heredoc…）必须跨行延续。逐行新建会让第二行起全部退回
    // 默认前景——`/* … */` 只有第一行还是注释色。
    let mut highlighter = HighlightLines::new(syntax_for(lang, syntax_set), syntax_theme);
    let mut out = Vec::new();
    for raw in text.lines() {
        // 空行也要喂给 highlighter，否则块注释里的空行会把状态吃掉。
        let spans = highlighted_spans(&mut highlighter, raw, syntax_set, theme);
        if spans.is_empty() {
            out.push(Line::from(Span::styled(
                CODE_INDENT.to_string(),
                Style::default(),
            )));
            continue;
        }
        let rows = wrap_styled(&spans, content_width);
        for row in rows {
            let mut line = Vec::with_capacity(row.len() + 1);
            line.push(Span::styled(CODE_INDENT.to_string(), Style::default()));
            line.extend(row.into_iter().map(StyledSpan::into_span));
            out.push(Line::from(line));
        }
    }
    if out.is_empty() {
        out.push(Line::from(Span::styled(
            CODE_INDENT.to_string(),
            Style::default(),
        )));
    }
    out
}

/// 围栏 info string 是否属于生成式 UI（`qaqh-ui` / `qaqh-ui v=0.2 format=tsx`）。
///
/// 只认**第一个 token**（与 [`syntax_for`] 同一口径）：`qaqh-ui v=0.2 format=jsonl`
/// 的第一个 token 是 `qaqh-ui`，大小写不敏感。
fn generative_ui_info(lang: Option<&str>) -> Option<String> {
    let lang = lang?;
    let mut tokens = lang
        .split(|ch: char| ch == ',' || ch.is_whitespace())
        .filter(|token| !token.is_empty());
    let first = tokens.next()?;
    if !first.eq_ignore_ascii_case("qaqh-ui") {
        return None;
    }
    let info = lang.trim();
    Some(if info.is_empty() {
        "qaqh-ui".to_string()
    } else {
        info.to_string()
    })
}

/// 生成式 UI 围栏的**保底卡片**。
///
/// 取舍写在 handoff §6.2：本端暂时不支持渲染交互组件（组件目录、表达式求值、
/// 动作回传都是项目级工作），但必须让用户知道「这里本来有界面」，而不是看到
/// 一整段 `ui.surface(...)` 模板源码——那既不像内容，也不像错误。
fn render_generative_ui_placeholder(
    text: &str,
    info: &str,
    width: usize,
    theme: &Theme,
) -> Vec<Line<'static>> {
    let content_width = width.saturating_sub(CODE_INDENT.width() + 3).max(8);
    let rows = text.lines().filter(|line| !line.trim().is_empty()).count();
    let heading = format!("生成式 UI（本端暂不支持渲染）· {info}");
    let body = format!("交互组件已折叠（{rows} 行 DSL）。完整渲染能力尚未在本端落地。");
    let mut out = Vec::new();
    for (index, line) in [heading, body].into_iter().enumerate() {
        let style = if index == 0 {
            fg(theme.accent.assistant).add_modifier(Modifier::BOLD)
        } else {
            fg(theme.text.dim)
        };
        let rows = wrap_styled(&[StyledSpan::new(line, style)], content_width);
        for (row_index, row) in rows.into_iter().enumerate() {
            let mut spans = Vec::with_capacity(row.len() + 1);
            // 只给首行画左边框：整段每行都画会显得像代码块，反而暗示"可以读"。
            spans.push(Span::styled(
                if index == 0 && row_index == 0 {
                    format!("{CODE_INDENT}▛ ")
                } else {
                    format!("{CODE_INDENT}  ")
                },
                fg(theme.accent.assistant),
            ));
            spans.extend(row.into_iter().map(StyledSpan::into_span));
            out.push(Line::from(spans));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme::{ColorSupport, ThemeKind};

    fn theme() -> Theme {
        Theme::resolve(ThemeKind::QaqhNight, ColorSupport::TrueColor)
    }

    fn text_of(lines: &[Line<'static>]) -> String {
        lines
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// 后端默认发的就是 v=0.2 围栏：两种 format 都必须被认出来，且**原文不上屏**。
    #[test]
    fn generative_ui_fences_fold_into_a_placeholder() {
        for info in [
            "qaqh-ui v=0.2 format=tsx",
            "qaqh-ui v=0.2 format=jsonl",
            "QAQH-UI",
        ] {
            let lines = render_code_block(
                "ui.surface(\"main\", { root: \"card\" })\nui.end()",
                Some(info),
                70,
                &theme(),
            );
            let text = text_of(&lines);
            assert!(text.contains("生成式 UI"), "{info} → {text}");
            assert!(!text.contains("ui.surface"), "DSL 原文不得上屏：{text}");
            assert!(!text.contains("ui.end"), "DSL 原文不得上屏：{text}");
        }
    }

    /// 普通代码块不受影响（尤其 `qaqh-ui2` 这类前缀相似的名字）。
    #[test]
    fn ordinary_code_blocks_still_highlight() {
        let lines = render_code_block("let x = 1;", Some("rust"), 60, &theme());
        let text = text_of(&lines);
        assert!(text.contains("let x = 1;"));
        assert!(!text.contains("生成式 UI"));
        let near = render_code_block("x", Some("qaqh-ui-extra"), 60, &theme());
        assert!(!text_of(&near).contains("生成式 UI"), "只认精确的 qaqh-ui");
    }
}

/// ```lang 的 info string → 语法。
///
/// 只取第一个 token，其余当参数忽略：GitHub 惯例里 ```rust,no_run、
/// ```rust ignore、```rust title="x" 都指 rust。整串拿去查表会一个都命中不了，
/// 于是**静默退成纯文本**——一行高亮都没有，看起来像渲染器坏了。
fn syntax_for<'a>(lang: Option<&str>, syntax_set: &'a SyntaxSet) -> &'a SyntaxReference {
    let token = lang.and_then(|lang| {
        lang.split(|ch: char| ch == ',' || ch.is_whitespace())
            .find(|token| !token.is_empty())
    });
    // `find_syntax_by_token` 本身已经是「先扩展名、再大小写不敏感的名字」。
    token
        .and_then(|token| syntax_set.find_syntax_by_token(token))
        .unwrap_or_else(|| syntax_set.find_syntax_plain_text())
}

fn highlighted_spans(
    highlighter: &mut HighlightLines<'_>,
    line: &str,
    syntax_set: &SyntaxSet,
    theme: &Theme,
) -> Vec<StyledSpan> {
    match highlighter.highlight_line(line, syntax_set) {
        Ok(ranges) => ranges
            .into_iter()
            .filter(|(_, text)| !text.is_empty())
            .map(|(style, text)| StyledSpan::new(text.to_string(), syntect_style(style, theme)))
            .collect(),
        Err(_) => vec![StyledSpan::new(line.to_string(), fg(theme.markdown.code))],
    }
}

/// base16-ocean 的 16 个基色（syntect 内置 `base16-ocean.dark` 的全部色板）。
const BASE16_OCEAN: [(u8, u8, u8); 16] = [
    (0x2b, 0x30, 0x3b), // 00 背景
    (0x34, 0x3d, 0x46), // 01
    (0x4f, 0x5b, 0x66), // 02
    (0x65, 0x73, 0x7e), // 03 注释
    (0xa7, 0xad, 0xba), // 04
    (0xc0, 0xc5, 0xce), // 05 默认前景
    (0xdf, 0xe1, 0xe8), // 06
    (0xef, 0xf1, 0xf5), // 07
    (0xbf, 0x61, 0x6a), // 08 红：变量 / 删除
    (0xd0, 0x87, 0x70), // 09 橙：数字 / 常量
    (0xeb, 0xcb, 0x8b), // 0a 黄：类 / 搜索
    (0xa3, 0xbe, 0x8c), // 0b 绿：字符串 / 新增
    (0x96, 0xb5, 0xb4), // 0c 青：支持 / 正则
    (0x8f, 0xa1, 0xb3), // 0d 蓝：函数 / 方法
    (0xb4, 0x8e, 0xad), // 0e 紫：关键字
    (0xab, 0x79, 0x67), // 0f 棕：内嵌 / 废弃
];

/// syntect 前景 → 主题 token。
///
/// syntect 只负责回答"这一段属于哪个语法 scope"；颜色一律经 base16-ocean 基色
/// 就近取整后回到 [`Theme`]。直接搬运 syntect 的 RGB 会绕过 `ColorSupport`
/// 降级与 `NO_COLOR`（16 色 / 无彩色终端上漏出真彩色），也会在亮色主题上铺一层
/// 为暗底设计的浅灰前景——`#c0c5ce` 压在 `#ffffff` 上只有约 1.6:1，等于看不见。
/// 主题 token 在解析期已完成能力降级，因此这里不再产生裸 RGB。
fn syntect_foreground(color: syntect::highlighting::Color, theme: &Theme) -> Color {
    let mut best = 5usize;
    let mut best_distance = u32::MAX;
    for (index, (r, g, b)) in BASE16_OCEAN.iter().enumerate() {
        let dr = i32::from(color.r) - i32::from(*r);
        let dg = i32::from(color.g) - i32::from(*g);
        let db = i32::from(color.b) - i32::from(*b);
        let distance = (dr * dr + dg * dg + db * db) as u32;
        if distance < best_distance {
            best_distance = distance;
            best = index;
        }
    }
    match best {
        0..=2 => theme.text.dim,
        3 | 4 => theme.text.muted,
        5 => theme.markdown.text,
        6 | 7 => theme.text.bright,
        8 => theme.accent.error,
        9 => theme.semantic.path,
        10 => theme.semantic.command,
        11 => theme.markdown.task_done,
        12 => theme.markdown.code,
        13 => theme.markdown.link,
        14 => theme.accent.assistant,
        _ => theme.semantic.warning,
    }
}

fn syntect_style(style: syntect::highlighting::Style, theme: &Theme) -> Style {
    let mut out = fg(syntect_foreground(style.foreground, theme));
    if style.font_style.contains(FontStyle::BOLD) {
        out = out.add_modifier(Modifier::BOLD);
    }
    if style.font_style.contains(FontStyle::ITALIC) {
        out = out.add_modifier(Modifier::ITALIC);
    }
    if style.font_style.contains(FontStyle::UNDERLINE) {
        out = out.add_modifier(Modifier::UNDERLINED);
    }
    out
}
