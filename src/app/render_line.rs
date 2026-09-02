//! 渲染 IR：与 ratatui 解耦的样式化行模型。
//!
//! transcript 渲染器产出 `Vec<RenderLine>`（按 model.version+宽度缓存），
//! UI 层按 theme 映射为 ratatui Line。这样逻辑换行/截断只算一次。

use unicode_width::UnicodeWidthChar;

/// 语义样式（theme.rs 映射到 ratatui Style）。Italic/Inverse 保留作调色板扩展。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
pub enum SpanStyle {
    Plain,
    Dim,
    Reasoning,
    Italic,
    Bold,
    User,
    Accent,
    ToolRun,
    ToolOk,
    ToolFail,
    Warn,
    Error,
    DiffAdd,
    DiffDel,
    Inverse,
    // Markdown 扩展（首版：标题/粗斜/行内码/代码块/链接/引用/分割线/表格）
    MdH1,
    MdH2,
    MdH3,
    MdInlineCode,
    MdCodeBlock,
    MdLink,
    MdQuote,
    MdRuler,
    MdTableHead,
    MdTableCell,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RenderStyle {
    Semantic(SpanStyle),
    Direct(ratatui::style::Style),
}

impl From<SpanStyle> for RenderStyle {
    fn from(s: SpanStyle) -> Self {
        Self::Semantic(s)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenderSpan {
    pub text: String,
    pub style: RenderStyle,
}

impl RenderSpan {
    pub fn new(text: impl Into<String>, style: SpanStyle) -> Self {
        Self {
            text: text.into(),
            style: RenderStyle::Semantic(style),
        }
    }
    pub fn with_style(text: impl Into<String>, style: ratatui::style::Style) -> Self {
        Self {
            text: text.into(),
            style: RenderStyle::Direct(style),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RenderLine {
    pub spans: Vec<RenderSpan>,
}

impl RenderLine {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn span(mut self, text: impl Into<String>, style: SpanStyle) -> Self {
        self.spans.push(RenderSpan::new(text, style));
        self
    }

    pub fn span_direct(mut self, text: impl Into<String>, style: ratatui::style::Style) -> Self {
        self.spans.push(RenderSpan::with_style(text, style));
        self
    }

    pub fn plain(text: impl Into<String>) -> Self {
        Self::new().span(text, SpanStyle::Plain)
    }

    #[allow(dead_code)]
    pub fn dim(text: impl Into<String>) -> Self {
        Self::new().span(text, SpanStyle::Dim)
    }

    #[allow(dead_code)]
    pub fn display_width(&self) -> usize {
        use unicode_width::UnicodeWidthStr;
        self.spans.iter().map(|s| s.text.width()).sum()
    }
}

/// 按显示宽度贪心折行（CJK 感知：优先在空格断行，超长 token 硬断）。
pub fn wrap_text(text: &str, width: usize) -> Vec<String> {
    use unicode_width::UnicodeWidthChar;
    if width == 0 {
        return vec![text.to_owned()];
    }
    let mut out: Vec<String> = Vec::new();
    for para in text.split('\n') {
        if para.is_empty() {
            out.push(String::new());
            continue;
        }
        let mut line = String::new();
        let mut line_w: usize = 0;
        let mut last_space: Option<(usize, usize)> = None; // (char 下标, 累计宽度)
        for ch in para.chars() {
            let cw = ch.width().unwrap_or(0);
            if line_w + cw > width {
                if ch == ' ' {
                    // 行尾空格本身就是换行点：丢弃即可，不必断词。
                    out.push(std::mem::take(&mut line));
                    line_w = 0;
                    last_space = None;
                    continue;
                }
                if let Some((space_idx, _space_w)) = last_space {
                    // 从最后空格处断行。
                    let head: String = line.chars().take(space_idx).collect();
                    let tail: String = line.chars().skip(space_idx + 1).collect();
                    out.push(head);
                    line = tail;
                    line_w = line.chars().map(|c| c.width().unwrap_or(0)).sum();
                    last_space = None;
                    // 当前字符重新尝试放入新行。
                    if line_w + cw <= width {
                        line.push(ch);
                        line_w += cw;
                        continue;
                    }
                }
                out.push(std::mem::take(&mut line));
                line_w = 0;
                last_space = None;
            }
            if ch == ' ' {
                last_space = Some((line.chars().count(), line_w));
            }
            line.push(ch);
            line_w += cw;
        }
        out.push(line);
    }
    if out.is_empty() {
        out.push(String::new());
    }
    out
}

/// 编辑缓冲的可视窗口：返回 (窗口文本, 光标在窗口内的列偏移)。
/// 保证光标列恒在窗口内（`off + 1 <= max_w`），供单行输入框水平滚动。
/// 从 ui/settings.rs 提升为共享（composer 与 settings 编辑态共用）。
pub(crate) fn edit_window(buf: &[char], cursor: usize, max_w: usize) -> (String, usize) {
    let w_of = |c: char| c.width().unwrap_or(0);
    let cursor = cursor.min(buf.len());
    let mut start = 0usize;
    loop {
        let off: usize = buf[start..cursor].iter().copied().map(w_of).sum();
        if off + 1 > max_w && start < cursor {
            start += 1;
        } else {
            let mut s = String::new();
            let mut used = 0usize;
            for &c in &buf[start..] {
                let w = w_of(c);
                if used + w > max_w {
                    break;
                }
                s.push(c);
                used += w;
            }
            let cursor_off: usize = buf[start..cursor].iter().copied().map(w_of).sum();
            return (s, cursor_off.min(used));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wrap_respects_width_and_spaces() {
        let lines = wrap_text("hello world foo", 11);
        assert_eq!(lines, vec!["hello world", "foo"]);
    }

    #[test]
    fn wrap_cjk_hard_breaks() {
        // 每个汉字宽 2：一行只放得下 3 个。
        let lines = wrap_text("你好世界测试", 6);
        assert_eq!(lines.join("\n").chars().filter(|c| *c != '\n').count(), 6);
        assert!(lines.iter().all(|l| l.chars().count() <= 3));
    }

    #[test]
    fn wrap_keeps_newlines() {
        let lines = wrap_text("a\n\nb", 10);
        assert_eq!(lines, vec!["a", "", "b"]);
    }

    #[test]
    fn render_line_width() {
        let l = RenderLine::new()
            .span("中文", SpanStyle::Plain)
            .span("ab", SpanStyle::Dim);
        assert_eq!(l.display_width(), 6);
    }

    #[test]
    fn edit_window_keeps_cursor_visible() {
        let buf: Vec<char> = "https://opencode.ai/zen/go/v1".chars().collect();
        // 光标在末尾：窗口截到最右，光标格占最后一列（偏移 = max-1）。
        let (s, off) = edit_window(&buf, buf.len(), 10);
        assert_eq!(s.chars().count(), 9);
        assert_eq!(off, 9);
        // 光标在开头：窗口从头开始。
        let (s, off) = edit_window(&buf, 0, 10);
        assert!(s.starts_with("https://"));
        assert_eq!(off, 0);
        // CJK：按宽度计算窗口；光标前 8 列放不下则窗口左移一字。
        let cjk: Vec<char> = "自动压缩阈值配置".chars().collect();
        let (s, off) = edit_window(&cjk, 4, 8);
        assert_eq!(s, "动压缩阈");
        assert_eq!(off, 6);
    }
}
