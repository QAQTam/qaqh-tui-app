//! 终端剪贴板输出。
//!
//! 全屏 TUI 开启了鼠标捕获，不能再依赖终端原生拖选。复制路径优先使用 OSC 52：
//! 它不依赖 X11/Wayland，也能穿过 SSH 到最终终端；是否真正写入系统剪贴板由
//! 终端实现决定。

use std::io::{Write, stdout};

use base64::Engine as _;

fn osc52_sequence(text: &str) -> String {
    let encoded = base64::engine::general_purpose::STANDARD.encode(text.as_bytes());
    format!("\x1b]52;c;{encoded}\x07")
}

pub fn copy_osc52(text: &str) -> std::io::Result<()> {
    if text.is_empty() {
        return Ok(());
    }
    let mut out = stdout().lock();
    out.write_all(osc52_sequence(text).as_bytes())?;
    out.flush()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn osc52_uses_clipboard_selection_and_bel_terminator() {
        assert_eq!(osc52_sequence("hi"), "\x1b]52;c;aGk=\x07");
    }
}
