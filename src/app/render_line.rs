//! 行布局工具：按终端显示宽度折行、单行编辑窗口。

use unicode_width::UnicodeWidthChar;

/// 按显示宽度贪心折行（CJK 感知：优先在空格断行，超长 token 硬断）。
pub fn wrap_text(text: &str, width: usize) -> Vec<String> {
    if width == 0 {
        return vec![text.to_owned()];
    }
    let mut state = WrapState::new(width);
    state.push_str(text);
    state.into_lines()
}

#[derive(Debug)]
struct WrapState {
    width: usize,
    sealed: Vec<String>,
    line: String,
    line_w: usize,
    last_space: Option<(usize, usize)>,
}

impl WrapState {
    fn new(width: usize) -> Self {
        assert!(width > 0);
        Self {
            width,
            sealed: Vec::new(),
            line: String::new(),
            line_w: 0,
            last_space: None,
        }
    }

    fn push_str(&mut self, text: &str) {
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

    fn step(&mut self, ch: char) {
        let cw = ch.width().unwrap_or(0);
        if self.line_w + cw > self.width {
            if ch == ' ' {
                self.sealed.push(std::mem::take(&mut self.line));
                self.line_w = 0;
                self.last_space = None;
                return;
            }
            if let Some((space_idx, _)) = self.last_space {
                let head: String = self.line.chars().take(space_idx).collect();
                let tail: String = self.line.chars().skip(space_idx + 1).collect();
                self.sealed.push(head);
                self.line = tail;
                self.line_w = self.line.chars().map(|c| c.width().unwrap_or(0)).sum();
                self.last_space = None;
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

    fn into_lines(mut self) -> Vec<String> {
        self.sealed.push(std::mem::take(&mut self.line));
        self.sealed
    }
}

/// 编辑缓冲的可视窗口：返回 `(窗口文本, 光标在窗口内的列偏移)`。
pub(crate) fn edit_window(buf: &[char], cursor: usize, max_w: usize) -> (String, usize) {
    let width = |c: char| c.width().unwrap_or(0);
    let cursor = cursor.min(buf.len());
    let mut start = 0usize;
    loop {
        let offset: usize = buf[start..cursor].iter().copied().map(width).sum();
        if offset + 1 > max_w && start < cursor {
            start += 1;
            continue;
        }
        let mut text = String::new();
        let mut used = 0usize;
        for &ch in &buf[start..] {
            let w = width(ch);
            if used + w > max_w {
                break;
            }
            text.push(ch);
            used += w;
        }
        let cursor_offset: usize = buf[start..cursor].iter().copied().map(width).sum();
        return (text, cursor_offset.min(used));
    }
}

#[cfg(test)]
mod tests {
    use super::{edit_window, wrap_text};

    #[test]
    fn wrap_text_handles_ascii_cjk_and_blank_lines() {
        assert_eq!(wrap_text("hello world foo", 11), ["hello world", "foo"]);
        assert_eq!(wrap_text("你好世界测试", 6), ["你好世", "界测试"]);
        assert_eq!(wrap_text("a\n\nb", 10), ["a", "", "b"]);
        assert_eq!(wrap_text("abc", 0), ["abc"]);
    }

    #[test]
    fn edit_window_keeps_cursor_visible() {
        let buf: Vec<char> = "hello world".chars().collect();
        let (text, offset) = edit_window(&buf, buf.len(), 6);
        assert_eq!(offset, 5);
        assert_eq!(text, "world");

        let (text, offset) = edit_window(&buf, 0, 6);
        assert_eq!(offset, 0);
        assert_eq!(text, "hello ");

        let cjk: Vec<char> = "你好世界".chars().collect();
        let (text, offset) = edit_window(&cjk, 4, 8);
        assert_eq!(offset, 6);
        assert_eq!(text, "好世界");
    }
}
