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

    pub fn display_width(&self) -> usize {
        use unicode_width::UnicodeWidthStr;
        self.spans.iter().map(|s| s.text.width()).sum()
    }
}

/// 按显示宽度贪心折行（CJK 感知：优先在空格断行，超长 token 硬断）。
///
/// 实现委托 [`WrapState`]（单源）：`wrap_text(text)` ≡ 状态机喂入全部文本后
/// 的行序列——流式增量的逐行等价由同一算法结构性成立（T7）。
pub fn wrap_text(text: &str, width: usize) -> Vec<String> {
    if width == 0 {
        return vec![text.to_owned()];
    }
    let mut ws = WrapState::new(width);
    ws.push_str(text);
    ws.into_lines()
}

/// 贪心折行状态机（[`wrap_text`] 的可续算内核，T7 流式尾部增量）。
///
/// - `wrap_text(text)` ≡ `WrapState::new(w)` 喂入全部 text 后的 [`WrapState::lines`]；
/// - 已封口行跨 delta 不再变化（贪心折行只影响当前行）——流式路径据此共享前缀；
/// - `width == 0` 不可用：wrap_text 对 0 有「整段直返」特例，而状态机需要
///   ≥1 的可用宽度（transcript 管线宽度恒 ≥ 20，`refresh` 已 clamp）。
///
/// 回归锁：`wrap_stream_incremental_matches_one_shot`（分片喂入 ≡ 一次性折行，
/// 含 ▌ 视图分解）。
#[derive(Debug, Clone)]
pub struct WrapState {
    width: usize,
    /// 已封口行（跨 delta 不再变化）。
    sealed: Vec<String>,
    /// 正在构建的尾行。
    line: String,
    line_w: usize,
    /// (行内 char 下标, 该处之前的累计显示宽)——断行点回溯用。
    last_space: Option<(usize, usize)>,
}

impl WrapState {
    pub fn new(width: usize) -> Self {
        assert!(
            width > 0,
            "WrapState 需要正宽度（wrap_text 的 0 宽特例不经此路径）"
        );
        Self {
            width,
            sealed: Vec::new(),
            line: String::new(),
            line_w: 0,
            last_space: None,
        }
    }

    pub fn width(&self) -> usize {
        self.width
    }

    /// 喂入文本（可含 '\n'；段落收口语义与 wrap_text 逐行一致：当前行即使
    /// 为空也封口，空段落产出空行）。
    pub fn push_str(&mut self, text: &str) {
        for ch in text.chars() {
            if ch == '\n' {
                self.sealed.push(std::mem::take(&mut self.line));
                self.line_w = 0;
                self.last_space = None;
                continue;
            }
            self.step(ch);
        }
    }

    /// 单字符步进（与旧 wrap_text 内层循环逐分支一致）。
    fn step(&mut self, ch: char) {
        let cw = ch.width().unwrap_or(0);
        if self.line_w + cw > self.width {
            if ch == ' ' {
                // 行尾空格本身就是换行点：丢弃即可，不必断词。
                self.sealed.push(std::mem::take(&mut self.line));
                self.line_w = 0;
                self.last_space = None;
                return;
            }
            if let Some((space_idx, _space_w)) = self.last_space {
                // 从最后空格处断行。
                let head: String = self.line.chars().take(space_idx).collect();
                let tail: String = self.line.chars().skip(space_idx + 1).collect();
                self.sealed.push(head);
                self.line = tail;
                self.line_w = self.line.chars().map(|c| c.width().unwrap_or(0)).sum();
                self.last_space = None;
                // 当前字符重新尝试放入新行。
                if self.line_w + cw <= self.width {
                    self.line.push(ch);
                    self.line_w += cw;
                    return;
                }
            }
            self.sealed.push(std::mem::take(&mut self.line));
            self.line_w = 0;
            self.last_space = None;
        }
        if ch == ' ' {
            self.last_space = Some((self.line.chars().count(), self.line_w));
        }
        self.line.push(ch);
        self.line_w += cw;
    }

    /// 当前快照 ≡ wrap_text(已喂文本)（末行即使为空也在列）。
    /// 测试/调试专用（生产出口为 [`Self::into_lines`]，增量共享只走 sealed）。
    #[cfg(test)]
    pub fn lines(&self) -> Vec<String> {
        let mut out = self.sealed.clone();
        out.push(self.line.clone());
        out
    }

    /// 消费状态取全部行（wrap_text 的出口）。
    pub fn into_lines(mut self) -> Vec<String> {
        self.sealed.push(std::mem::take(&mut self.line));
        self.sealed
    }

    /// 已封口行数（流式前缀的长度）。
    pub fn sealed_len(&self) -> usize {
        self.sealed.len()
    }

    /// 已封口行切片（流式前缀物化用）。
    pub fn sealed_slice(&self) -> &[String] {
        &self.sealed
    }

    /// ▌ 光标视图（T7）：把光标按**同一 step 规则**虚拟喂进当前行。
    ///
    /// 返回 `(被 ▌ 折出的封口行, 以 ▌ 结尾的当前行)`；与
    /// `wrap_text(text + "▌")` 的分解严格一致——光标只与当前行交互，
    /// 已封口前缀不受影响（▌ 宽 1，ls 断行后的 retry 恒放得下：
    /// tail_w ≤ width-1，tail_w+1 ≤ width）。
    pub fn cursor_view(&self) -> (Option<String>, String) {
        let cw = '▌'.width().unwrap_or(1);
        let mut line = self.line.clone();
        let line_w = self.line_w;
        if line_w + cw > self.width {
            if let Some((space_idx, _)) = self.last_space {
                let head: String = line.chars().take(space_idx).collect();
                let tail: String = line.chars().skip(space_idx + 1).collect();
                let mut line = tail;
                line.push('▌');
                return (Some(head), line);
            }
            // 硬断行：整行封口，光标独占新行。
            return (Some(std::mem::take(&mut line)), String::from("▌"));
        }
        line.push('▌');
        (None, line)
    }
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

    /// T7 锁：分片喂入 ≡ 一次性 wrap_text（每一步逐行一致）；▌ 视图分解 ≡
    /// wrap_text(text_so_far + "▌") 的 sealed 前缀 + 尾部（流式显示的等价性
    /// 由本锁结构化保证，锁 6 的 seal 等价是它的端到端推论）。
    #[test]
    fn wrap_stream_incremental_matches_one_shot() {
        let cases: [(&str, usize, &[&str]); 6] = [
            ("hello world", 80, &["hello", " world"]),
            ("压缩压缩压缩压缩压缩", 6, &["压缩", "压缩", "压缩"]),
            ("abc", 3, &["a", "b", "c", ""]),
            // ▌ 硬断行 + 空格开头的后续 delta（曾实测分叉的形状）。
            ("abc d", 3, &["abc", " d"]),
            ("a\n\nb\n", 10, &["a\n", "\nb\n"]),
            ("ab c", 3, &["ab", " c"]),
        ];
        for (text, width, chunks) in cases {
            let mut ws = WrapState::new(width);
            let mut fed = String::new();
            for chunk in chunks {
                ws.push_str(chunk);
                fed.push_str(chunk);
                assert_eq!(
                    ws.lines(),
                    wrap_text(&fed, width),
                    "增量 ≡ 一次性：text={text:?} fed={fed:?}"
                );
                // ▌ 视图分解：one-shot(text+▌) = sealed 前缀 ++ (overflow, cur)。
                let mut expect = wrap_text(&format!("{fed}▌"), width);
                let (overflow, cur) = ws.cursor_view();
                let tail_len = 1 + usize::from(overflow.is_some());
                let expect_tail = expect.split_off(expect.len() - tail_len);
                let mut actual_tail = Vec::with_capacity(tail_len);
                if let Some(o) = &overflow {
                    actual_tail.push(o.clone());
                }
                actual_tail.push(cur.clone());
                assert_eq!(
                    expect_tail, actual_tail,
                    "▌ 视图：text={text:?} fed={fed:?}"
                );
                assert_eq!(
                    &expect,
                    &ws.lines()[..ws.sealed_len()],
                    "sealed 前缀应与 one-shot 一致：fed={fed:?}"
                );
            }
        }
    }

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
