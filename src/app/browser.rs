//! 用**系统浏览器**打开授权页 / 深链。
//!
//! TUI 没有内嵌 webview，登录的最后一公里就是把 daemon 给的 `authorizeUrl`
//! 交给浏览器（契约：`spec-service-surface-siwc-dsaccount.md` §3.5「浏览器属宿主」）。
//!
//! 两条纪律：
//! 1. **只放行 http/https**（与桌面壳 `open_external` 同口径）：`file:` / `javascript:`
//!    之类的 scheme 一旦被带到浏览器就是本地文件读取或 XSS 入口，而 URL 来自网络。
//! 2. **不拼 shell 字符串**：每个参数单独交给 `Command`，不做 `format!` 拼命令行
//!    （空格与引号不该成为注入面）。

/// URL 是否允许交给浏览器。
pub fn is_supported(url: &str) -> bool {
    let url = url.trim();
    if url.is_empty() || url.chars().any(|c| c.is_control() || c.is_whitespace()) {
        return false;
    }
    let lower = url.to_ascii_lowercase();
    let http = lower.starts_with("http://") && lower.len() > "http://".len();
    let https = lower.starts_with("https://") && lower.len() > "https://".len();
    http || https
}

/// 在系统浏览器里打开 `url`。
///
/// 返回时**浏览器已经启动**（spawn 成功），但不等它退出——用户可能读十分钟。
pub fn open(url: &str) -> Result<(), String> {
    let url = url.trim();
    if !is_supported(url) {
        return Err(format!("拒绝打开非 http(s) 链接：{url}"));
    }
    #[cfg(target_os = "windows")]
    let mut command = {
        // `start` 的第一个带引号参数是窗口标题：必须显式给空串，
        // 否则 URL 会被当成标题吞掉（Windows 上最经典的「点了没反应」）。
        let mut command = std::process::Command::new("cmd");
        command.args(["/C", "start", ""]);
        command
    };
    #[cfg(target_os = "macos")]
    let mut command = std::process::Command::new("open");
    #[cfg(all(unix, not(target_os = "macos")))]
    let mut command = std::process::Command::new("xdg-open");

    let mut child = command
        .arg(url)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map_err(|e| format!("启动浏览器失败：{e}"))?;
    // 不阻塞界面：回收句柄交给一个短线程，避免僵尸进程（Unix）与句柄泄漏。
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_http_and_https_survive_the_gate() {
        assert!(is_supported("https://auth.openai.com/oauth?x=1"));
        assert!(is_supported("http://127.0.0.1:1455/callback"));
        assert!(!is_supported("file:///etc/passwd"));
        assert!(!is_supported("javascript:alert(1)"));
        assert!(!is_supported("https://"));
        assert!(!is_supported(""));
        assert!(!is_supported("   "));
    }

    /// 空白/换行会改变命令行语义（Windows 上等价于再塞一个参数）。
    #[test]
    fn whitespace_and_control_characters_are_rejected() {
        assert!(!is_supported("https://a b.c"));
        assert!(!is_supported("https://a\nb.c"));
        assert!(!is_supported("https://a\tb.c"));
    }

    #[test]
    fn opening_a_rejected_scheme_never_spawns_anything() {
        let error = open("javascript:alert(1)").expect_err("必须拒绝");
        assert!(error.contains("非 http(s)"));
    }
}
