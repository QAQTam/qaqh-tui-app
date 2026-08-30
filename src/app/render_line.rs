//! 渲染 IR：与 ratatui 解耦的样式化行模型。
//!
//! transcript 渲染器产出 `Vec<RenderLine>`（按 model.version+宽度缓存），
//! UI 层按 theme 映射为 ratatui Line。这样逻辑换行/截断只算一次。

/// 语义样式（theme.rs 映射到 ratatui Style）。Italic/Inverse 保留作调色板扩展。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
pub enum SpanStyle {
    Plain,
    Dim,
    Reasoning,
    Italic,
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
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenderSpan {
    pub text: String,
    pub style: SpanStyle,
}

impl RenderSpan {
    pub fn new(text: impl Into<String>, style: SpanStyle) -> Self {
        Self { text: text.into(), style }
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
            let cw = ch.width().unwrap_or(0).max(0) as usize;
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
                    line_w = line.chars().map(|c| c.width().unwrap_or(0).max(0) as usize).sum();
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
        let l = RenderLine::new().span("中文", SpanStyle::Plain).span("ab", SpanStyle::Dim);
        assert_eq!(l.display_width(), 6);
    }
}
