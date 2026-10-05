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

#[test]
fn renders_inline_emphasis_without_markers() {
    let lines = render("**bold** and *italic* and ~~gone~~", 60, &theme());
    let text = text_of(&lines);
    assert_eq!(text, "bold and italic and gone");
    let spans: Vec<&Span<'_>> = lines.iter().flat_map(|line| line.spans.iter()).collect();
    assert!(
        spans
            .iter()
            .any(|span| span.style.add_modifier.contains(Modifier::BOLD))
    );
    assert!(
        spans
            .iter()
            .any(|span| span.style.add_modifier.contains(Modifier::ITALIC))
    );
    assert!(
        spans
            .iter()
            .any(|span| span.style.add_modifier.contains(Modifier::CROSSED_OUT))
    );
}

#[test]
fn renders_heading_levels_and_theme_colors() {
    let lines = render("# one\n\n#### four", 40, &theme());
    let heading_one = lines
        .iter()
        .flat_map(|line| line.spans.iter())
        .find(|span| span.content.as_ref() == "one")
        .expect("h1");
    let heading_four = lines
        .iter()
        .flat_map(|line| line.spans.iter())
        .find(|span| span.content.as_ref() == "four")
        .expect("h4");
    assert_eq!(heading_one.style.fg, Some(theme().markdown.h1));
    assert_eq!(heading_four.style.fg, Some(theme().markdown.h4));
}

#[test]
fn renders_links_with_link_style() {
    let lines = render("[OpenAI](https://openai.com)", 60, &theme());
    let text = text_of(&lines);
    assert_eq!(text, "OpenAI");
    let span = lines
        .iter()
        .flat_map(|line| line.spans.iter())
        .find(|span| span.content.as_ref() == "OpenAI")
        .expect("link text");
    assert_eq!(span.style.fg, Some(theme().markdown.link));
    assert!(span.style.add_modifier.contains(Modifier::UNDERLINED));
}

#[test]
fn renders_tables_and_keeps_rows() {
    let lines = render(
        "| name | value |\n|---|---|\n| a | 1 |\n| b | 2 |",
        40,
        &theme(),
    );
    let text = text_of(&lines);
    assert!(text.contains("name"));
    assert!(text.contains("value"));
    assert!(text.contains("a"));
    assert!(text.contains("1"));
    assert!(text.contains("b"));
    assert!(text.contains("2"));
    assert!(lines.len() >= 6);
}

/// 从 `┌───┬───┐` 这类顶边框量出各列宽度。
fn border_col_widths(line: &str) -> Vec<usize> {
    line.trim_start_matches('┌')
        .trim_end_matches('┐')
        .split('┬')
        .map(|seg| seg.chars().filter(|ch| *ch == '─').count())
        .collect()
}

#[test]
fn table_columns_follow_content_width() {
    let lines = render(
        "| 名 | 说明 |\n|---|---|\n| 甲 | 这是一段明显更长的说明文字 |",
        80,
        &theme(),
    );
    let top = text_of(&lines[..1]);
    assert!(top.starts_with('┌'), "首行应是顶边框: {top}");
    let widths = border_col_widths(&top);
    assert_eq!(widths.len(), 2);
    assert_eq!(widths[0], 3, "窄列拿到内容宽度即可");
    assert!(
        widths[1] > widths[0] * 5,
        "宽列应按内容拿到更多宽度: {widths:?}"
    );
    assert!(top.width() < 80, "表格不必撑满整行: {}", top.width());
}

#[test]
fn table_columns_shrink_the_widest_first() {
    let lines = render(
        "| 名 | 说明 |\n|---|---|\n| 甲 | 这是一段明显更长的说明文字 |",
        30,
        &theme(),
    );
    let widths = border_col_widths(&text_of(&lines[..1]));
    assert_eq!(widths[0], 3, "窄列不该被继续压缩");
    assert!(
        widths[1] > widths[0],
        "先让位的是宽列而不是均摊: {widths:?}"
    );
    assert_eq!(
        widths.iter().sum::<usize>() + widths.len() + 1,
        30,
        "放不下时用满可用宽度: {widths:?}"
    );
}

/// 单元格内的 `<br>` 是换行，不能被吞掉（grok `test_table_br_tag_becomes_line_break`）。
#[test]
fn table_br_tag_becomes_line_break() {
    let text = text_of(&render(
        "| 名 | 说明 |\n|---|---|\n| 甲 | 第一行<br>第二行<br>第三行 |",
        40,
        &theme(),
    ));
    assert!(!text.contains("第一行第二行"), "三段文字被粘连：\n{text}");
    for needle in ["第一行", "第二行", "第三行"] {
        assert!(text.contains(needle), "缺 {needle}：\n{text}");
    }
}

/// 超长中文按列宽折行，不截断、不超宽。
#[test]
fn table_cells_wrap_without_truncating() {
    let src = "| 名 | 说明 |\n|---|---|\n| 甲 | 这是一段特别长的中文说明文字用来观察折行 |";
    for pane in [24usize, 40, 60] {
        let lines = render(src, pane, &theme());
        let text = text_of(&lines);
        assert!(
            lines.iter().all(|line| line.width() <= pane),
            "pane={pane} 溢出：\n{text}"
        );
        assert!(!text.contains('…'), "pane={pane} 不该截断：\n{text}");
    }
}

/// 长 URL 优先在路径分隔处折行，而不是被省略号截断。
#[test]
fn table_wraps_long_url_at_path_boundaries() {
    let text = text_of(&render(
        "| 项 | 地址 |\n|---|---|\n| 文档 | https://example.com/a/very/long/path |",
        40,
        &theme(),
    ));
    assert!(text.contains("example.com/a/"), "未按路径折行：\n{text}");
    assert!(!text.contains('…'), "URL 被截断：\n{text}");
}

/// GFM alert（`> [!NOTE]`）渲染成带标签的引用块。
#[test]
fn alert_blockquote_gets_a_label() {
    let note = text_of(&render("> [!NOTE]\n> 记得先跑测试。", 40, &theme()));
    assert!(note.contains("NOTE"), "{note}");
    assert!(note.contains("记得先跑测试。"), "{note}");

    let warning = text_of(&render("> [!WARNING]\n> 小心。", 40, &theme()));
    assert!(warning.contains("WARNING"), "{warning}");

    // 普通引用块不额外加抬头。
    let plain = text_of(&render("> 普通引用", 40, &theme()));
    assert!(!plain.contains("NOTE"), "{plain}");
}

/// 列数超过上限时不静默丢列。
#[test]
fn table_column_overflow_is_announced() {
    let text = text_of(&render(
        "| c1 | c2 | c3 | c4 | c5 | c6 | c7 | c8 | c9 | c10 |\n|---|---|---|---|---|---|---|---|---|---|\n| 1 | 2 | 3 | 4 | 5 | 6 | 7 | 8 | 9 | 10 |",
        40,
        &theme(),
    ));
    assert!(text.contains("省略 1 列"), "丢列必须有提示：\n{text}");
}

#[test]
fn code_block_has_no_background_and_keeps_indent() {
    let lines = render("```rust\nfn main() {}\n```", 50, &theme());
    let text = text_of(&lines);
    assert!(text.contains("fn main() {}"));
    for line in &lines {
        for span in &line.spans {
            assert_eq!(span.style.bg, None, "code span must not paint background");
        }
    }
}

fn contrast_ratio(foreground: Color, background: Color) -> f64 {
    fn channel(value: u8) -> f64 {
        let value = f64::from(value) / 255.0;
        if value <= 0.040_45 {
            value / 12.92
        } else {
            ((value + 0.055) / 1.055).powf(2.4)
        }
    }
    fn luminance(color: Color) -> f64 {
        let Color::Rgb(r, g, b) = color else {
            panic!("contrast needs resolved RGB, got {color:?}");
        };
        0.2126 * channel(r) + 0.7152 * channel(g) + 0.0722 * channel(b)
    }
    let (a, b) = (luminance(foreground), luminance(background));
    let (lighter, darker) = if a > b { (a, b) } else { (b, a) };
    (lighter + 0.05) / (darker + 0.05)
}

/// 代码块高亮只能吐主题 token：不得再出现 base16-ocean 的裸 RGB。
/// 裸 RGB 会绕过 `ColorSupport` 降级与 `NO_COLOR`（16 色 / 无彩色终端上漏出
/// 真彩色），也会在亮色主题上铺一层为暗底设计的浅灰前景。
#[test]
fn code_block_highlights_resolve_to_theme_tokens() {
    for kind in [
        ThemeKind::QaqhNight,
        ThemeKind::QaqhDay,
        ThemeKind::Terminal,
    ] {
        for support in [
            ColorSupport::TrueColor,
            ColorSupport::Ansi256,
            ColorSupport::Ansi16,
            ColorSupport::NoColor,
        ] {
            let theme = Theme::resolve(kind, support);
            let allowed = [
                theme.text.dim,
                theme.text.muted,
                theme.text.bright,
                theme.markdown.text,
                theme.markdown.code,
                theme.markdown.link,
                theme.markdown.task_done,
                theme.accent.error,
                theme.accent.assistant,
                theme.semantic.command,
                theme.semantic.path,
                theme.semantic.warning,
            ];
            let lines = render("```rust\nfn main() { let x = 1; }\n```", 60, &theme);
            for line in &lines {
                for span in &line.spans {
                    let Some(color) = span.style.fg else {
                        continue;
                    };
                    assert!(
                        allowed.contains(&color),
                        "{kind:?}/{support:?} leaked {color:?} for {:?}",
                        span.content
                    );
                }
            }
        }
    }
}

/// 亮色主题下代码块必须是深色前景：旧实现无条件搬 base16-ocean 的
/// `#c0c5ce`，压在 day 主题的 `#f7f8fa` 上约 1.6:1，代码块基本看不见。
#[test]
fn day_code_block_foreground_stays_readable() {
    let theme = Theme::resolve(ThemeKind::QaqhDay, ColorSupport::TrueColor);
    let lines = render("```rust\nfn main() { let x = 1; }\n```", 60, &theme);
    for line in &lines {
        for span in &line.spans {
            let Some(color) = span.style.fg else {
                continue;
            };
            let ratio = contrast_ratio(color, theme.surface.base);
            assert!(
                ratio >= 3.0,
                "day code span {:?} = {color:?} contrast {ratio:.2}",
                span.content
            );
        }
    }
}

fn span_fg(lines: &[Line<'static>], needle: &str) -> Option<Color> {
    lines
        .iter()
        .flat_map(|line| line.spans.iter())
        .find(|span| span.content.contains(needle))
        .and_then(|span| span.style.fg)
}

/// 跨行解析状态：块注释从第二行起必须还是注释色。
///
/// 旧实现每行新建一台 `HighlightLines`，解析状态不跨行——`/* … */` 只有第一
/// 行是注释色，其余行全部退回默认前景。三引号字符串、模板字符串、heredoc
/// 同理。
#[test]
fn code_block_keeps_multiline_comment_state() {
    let theme = theme();
    let lines = render(
        "```rust\n/* alpha\n   bravo\n   charlie */\nlet x = 1;\n```",
        60,
        &theme,
    );
    assert_eq!(
        span_fg(&lines, "bravo"),
        Some(theme.text.muted),
        "块注释第二行必须仍是注释色"
    );
    assert_eq!(
        span_fg(&lines, "charlie"),
        Some(theme.text.muted),
        "块注释收尾行必须仍是注释色"
    );
}

/// 跨行状态同样要覆盖空行：注释中间的空行不能把解析状态吃掉。
#[test]
fn code_block_keeps_state_across_blank_lines() {
    let theme = theme();
    let lines = render("```rust\n/* alpha\n\n   bravo */\n```", 60, &theme);
    assert_eq!(
        span_fg(&lines, "bravo"),
        Some(theme.text.muted),
        "注释里的空行不能中断注释状态"
    );
}

/// ```lang 的 info string 带参数时必须仍按语言上色。
///
/// 旧实现把整串 `rust,no_run` 拿去查表 → 一个都命中不了 → 静默退成纯文本，
/// 一行高亮都没有。GitHub 惯例只取第一个 token。
#[test]
fn code_block_info_string_keeps_language() {
    let theme = theme();
    for fence in ["rust", "rs", "Rust", "rust,no_run", "rust ignore"] {
        let source = format!("```{fence}\nlet x = 1;\n```");
        let lines = render(&source, 60, &theme);
        let keyword =
            span_fg(&lines, "let").unwrap_or_else(|| panic!("no `let` span for `{fence}`"));
        assert_ne!(
            keyword, theme.markdown.text,
            "fence `{fence}` 没有按 rust 上色（退成了纯文本）"
        );
    }
}

/// 未知语言仍然安静地退成纯文本，不能 panic 也不能乱上色。
#[test]
fn code_block_unknown_language_falls_back_to_plain_text() {
    let theme = theme();
    let lines = render("```not-a-language\nlet x = 1;\n```", 60, &theme);
    assert_eq!(span_fg(&lines, "let"), Some(theme.markdown.text));
}

#[test]
fn wraps_cjk_without_splitting_wide_chars() {
    let lines = render("这是一段需要折行的中文文本，用于验证宽度。", 14, &theme());
    assert!(lines.iter().all(|line| line.width() <= 14));
    assert!(text_of(&lines).contains("中文文本"));
}

#[test]
fn empty_input_still_returns_one_line() {
    let lines = render("", 20, &theme());
    assert_eq!(lines.len(), 1);
    assert!(lines[0].spans.is_empty());
}
