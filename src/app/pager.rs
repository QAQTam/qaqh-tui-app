//! `$PAGER`（M4 / T15）：把只读浮层的内容交给外部分页器全文浏览。
//!
//! 终端挂起/恢复由 main.rs 主循环执行（只有那里持有 `Terminal`）；
//! 本模块只负责**命令解析**与**shell 引用**——纯函数，便于回归。

/// 解析 `$PAGER` → 命令行串。
///
/// 规则：未设置或空白 → `less -R`（保留 ANSI 颜色）；其余原样（用户可带参数，
/// 经 `sh -c` 执行）。
pub(crate) fn pager_cmd(raw: Option<&str>) -> String {
    raw.map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| "less -R".to_string())
}

/// 单引号安全引用（POSIX sh）：`'` → `'\''`。
pub(crate) fn shell_quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('\'');
    for c in s.chars() {
        if c == '\'' {
            out.push_str("'\\''");
        } else {
            out.push(c);
        }
    }
    out.push('\'');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pager_cmd_falls_back_to_less() {
        assert_eq!(pager_cmd(None), "less -R");
        assert_eq!(pager_cmd(Some("")), "less -R");
        assert_eq!(pager_cmd(Some("   ")), "less -R");
        assert_eq!(pager_cmd(Some("bat -p")), "bat -p");
        assert_eq!(pager_cmd(Some("  more  ")), "more");
    }

    #[test]
    fn shell_quote_escapes_single_quotes() {
        assert_eq!(shell_quote("/tmp/a.md"), "'/tmp/a.md'");
        assert_eq!(shell_quote("a'b"), "'a'\\''b'");
        assert_eq!(shell_quote("my dir/файл.md"), "'my dir/файл.md'");
    }
}
