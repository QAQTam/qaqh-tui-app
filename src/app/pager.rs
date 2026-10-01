//! `$PAGER`（M4 / T15）：把只读浮层的内容交给外部分页器全文浏览。
//!
//! 终端挂起/恢复由 main.rs 主循环执行（只有那里持有 `Terminal`）；
//! 本模块只负责**命令解析**、**shell 引用**与**临时文件安全**——纯函数，便于回归。

use std::io::Write as _;
use std::path::{Path, PathBuf};

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

/// 模型正文交给 `$PAGER` 前净化：剥离全部 ANSI/VT 转义。
///
/// 正文来自模型（不可信）。`less -R` 直通 SGR，`sh` 缺失时的 `cat` 回退更是
/// 原样输出——OSC 52 改剪贴板、OSC 8 伪造超链接、改标题等序列都会被真实
/// 终端解释（审计 F2）。正文本来就不含合法颜色码，宁可全剥。
pub(crate) fn sanitize_body(body: &str) -> String {
    crate::app::timeline_model::strip_ansi_escapes(body)
}

/// 唯一创建（`create_new` = `O_CREAT|O_EXCL`）并写入 pager 临时文件。
///
/// 不用 `fs::write`：可预测路径 + 跟随符号链接（CWE-377）——多用户主机上
/// 本地攻击者可预置 symlink，让写入覆盖任意可写文件（如 `~/.bashrc`）。
/// 这里持有**打开句柄**完成写入，不再经过路径名，create→write 之间的
/// swap 攻击同样不可行。文件名带纳秒时间戳，撞名只可能是恶意占位，
/// `AlreadyExists` 时换名重试。
pub(crate) fn write_temp_file(dir: &Path, body: &str) -> std::io::Result<PathBuf> {
    for _ in 0..16 {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or_default();
        let path = dir.join(format!("qaqh-pager-{}-{nanos}.md", std::process::id()));
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(mut file) => {
                file.write_all(body.as_bytes())?;
                return Ok(path);
            }
            // 别人（或上一次崩溃的残留）占了名字：换一个再试。
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::AlreadyExists,
        "pager 临时文件重试耗尽",
    ))
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

    /// 审计 F2 回归：交给 pager 的正文不得携带任何转义序列。
    #[test]
    fn sanitize_body_strips_osc_and_csi() {
        // OSC 52 剪贴板投毒 + OSC 8 伪链接 + CSI 私有模式。
        let poisoned = "\x1b]52;c;aGVsbG8=\x07see\x1b]8;;http://evil\x07text\x1b[?1049h";
        assert_eq!(sanitize_body(poisoned), "seetext");
        assert_eq!(sanitize_body("\x1b[31mred\x1b[0m"), "red");
        assert_eq!(sanitize_body("plain 中文"), "plain 中文");
    }

    /// 审计 F2 回归：临时文件必须独占创建（拒绝跟随已有文件/符号链接），
    /// 内容经句柄落盘，且每次调用得到不同路径。
    #[test]
    fn write_temp_file_creates_exclusive_and_unique() {
        let dir = std::env::temp_dir().join(format!(
            "qaqh-pager-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).expect("mkdir");

        let first = write_temp_file(&dir, "body-1").expect("first write");
        assert_eq!(std::fs::read_to_string(&first).unwrap(), "body-1");

        // 第二次调用不得复用/覆盖第一个文件。
        let second = write_temp_file(&dir, "body-2").expect("second write");
        assert_ne!(first, second);
        assert_eq!(std::fs::read_to_string(&first).unwrap(), "body-1");

        std::fs::remove_dir_all(&dir).ok();
    }
}
