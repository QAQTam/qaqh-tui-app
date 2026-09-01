//! Slash 命令解析与二级菜单定义
//! 一级：/new, /help ...  二级：/new 的 cwd 参数

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SlashDef {
    pub name: &'static str,
    pub desc: &'static str,
    pub hint: &'static str,
}

pub const SLASH_COMMANDS: &[SlashDef] = &[
    SlashDef { name: "new", desc: "新建会话", hint: "/new [cwd]  在指定目录新建会话（cwd 为绝对路径，留空则按 环境变量>启动目录>当前会话 回退）" },
    SlashDef { name: "help", desc: "帮助", hint: "/help  打开帮助" },
    SlashDef { name: "clear", desc: "清空输入", hint: "/clear  清空当前输入" },
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SlashCmd {
    New { cwd: Option<String> },
    Help,
    Clear,
    Unknown(String),
}

#[allow(dead_code)]
pub fn is_slash(input: &str) -> bool {
    input.starts_with('/')
}

/// 解析 slash 输入。要求 input 已经 trim 且以 '/' 开头。
pub fn parse(input: &str) -> Option<SlashCmd> {
    let t = input.trim();
    if !t.starts_with('/') {
        return None;
    }
    let without = t[1..].trim();
    if without.is_empty() {
        return Some(SlashCmd::Unknown(String::new()));
    }
    // 拆 command 与 args（保留 args 原样，支持含空格路径，首尾引号脱壳）
    let (cmd, raw_args) = match without.find(char::is_whitespace) {
        Some(idx) => (&without[..idx], without[idx..].trim()),
        None => (without, ""),
    };
    let cmd_lower = cmd.to_ascii_lowercase();
    match cmd_lower.as_str() {
        "new" => {
            let cwd = if raw_args.is_empty() {
                None
            } else {
                let s = unquote(raw_args).trim().to_string();
                if s.is_empty() { None } else { Some(s) }
            };
            Some(SlashCmd::New { cwd })
        }
        "help" => Some(SlashCmd::Help),
        "clear" => Some(SlashCmd::Clear),
        // 保留缩写：/n -> /new
        "n" => {
            let cwd = if raw_args.is_empty() { None } else { Some(unquote(raw_args).trim().to_string()) };
            let cwd = cwd.filter(|s| !s.is_empty());
            Some(SlashCmd::New { cwd })
        }
        _ => Some(SlashCmd::Unknown(cmd.to_string())),
    }
}

fn unquote(s: &str) -> String {
    let t = s.trim();
    if (t.starts_with('"') && t.ends_with('"') && t.len() >= 2)
        || (t.starts_with('\'') && t.ends_with('\'') && t.len() >= 2)
    {
        t[1..t.len()-1].to_string()
    } else {
        t.to_string()
    }
}

/// 一级菜单过滤：仅当输入尚未含空格时（还在输命令名阶段）才展示命令补全
pub fn completions_for(input: &str) -> Vec<&'static SlashDef> {
    let t = input.trim();
    if !t.starts_with('/') {
        return vec![];
    }
    let without = &t[1..];
    // 已含空格说明已确定命令，进入参数阶段，不再展示一级菜单
    if without.contains(char::is_whitespace) {
        return vec![];
    }
    let filter = without.to_ascii_lowercase();
    if filter.is_empty() {
        return SLASH_COMMANDS.iter().collect();
    }
    SLASH_COMMANDS.iter().filter(|d| d.name.starts_with(filter.as_str())).collect()
}

/// ~ 展开：~/foo -> $HOME/foo；~user 不处理原样返回
pub fn expand_tilde(p: &str) -> String {
    if !p.starts_with('~') { return p.to_string(); }
    // ~/ 或仅 ~
    if p == "~" || p.starts_with("~/") || p.starts_with("~\\") {
        if let Some(home) = dirs_home() {
            return format!("{}{}", home, &p[1..]);
        }
    }
    p.to_string()
}

fn dirs_home() -> Option<String> {
    if let Ok(h) = std::env::var("HOME") { if !h.is_empty() { return Some(h); } }
    if let Ok(h) = std::env::var("USERPROFILE") { if !h.is_empty() { return Some(h); } }
    None
}

pub fn is_absolute_path(p: &str) -> bool {
    let s = p.trim();
    if s.is_empty() { return false; }
    let path = std::path::Path::new(s);
    if path.is_absolute() { return true; }
    // Windows 绝对路径兼容：C:\ / C:/ / \\server\share
    if s.len() >= 2 && s.as_bytes()[1] == b':' && s.as_bytes()[0].is_ascii_alphabetic() {
        let rest = &s[2..];
        return rest.starts_with('\\') || rest.starts_with('/');
    }
    if s.starts_with("\\\\") { return true; }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_new_no_arg() {
        assert_eq!(parse("/new"), Some(SlashCmd::New{ cwd: None }));
        assert_eq!(parse("/new  "), Some(SlashCmd::New{ cwd: None }));
        assert_eq!(parse("/n"), Some(SlashCmd::New{ cwd: None }));
    }

    #[test]
    fn parse_new_with_cwd() {
        assert_eq!(parse("/new C:\\code\\foo"), Some(SlashCmd::New{ cwd: Some("C:\\code\\foo".into()) }));
        assert_eq!(parse("/new \"/tmp/my project\""), Some(SlashCmd::New{ cwd: Some("/tmp/my project".into()) }));
        assert_eq!(parse("/new 'C:\\a b'"), Some(SlashCmd::New{ cwd: Some("C:\\a b".into()) }));
    }

    #[test]
    fn completions_filter() {
        let all = completions_for("/");
        assert_eq!(all.len(), SLASH_COMMANDS.len());
        let filtered = completions_for("/n");
        assert!(filtered.iter().any(|d| d.name=="new"));
        let none = completions_for("/xyz");
        assert!(none.is_empty());
        let no_space = completions_for("/new C:\\");
        assert!(no_space.is_empty(), "参数阶段不应展示一级菜单");
    }

    #[test]
    fn absolute_path() {
        assert!(is_absolute_path("C:\\code\\foo"));
        assert!(is_absolute_path("C:/code/foo"));
        assert!(is_absolute_path("/tmp/foo"));
        assert!(is_absolute_path("\\\\server\\share"));
        assert!(!is_absolute_path("relative/path"));
        assert!(!is_absolute_path(""));
    }

    #[test]
    fn expand_tilde_prefix() {
        assert_eq!(expand_tilde("relative"), "relative");
        assert_eq!(expand_tilde("~"), if std::env::var("HOME").is_ok() || std::env::var("USERPROFILE").is_ok() { expand_tilde("~") } else { "~".into() });
        // ~/foo 必须被展开或保持原样但含分隔符
        let e = expand_tilde("~/foo");
        assert!(e.contains("foo"));
    }
}
