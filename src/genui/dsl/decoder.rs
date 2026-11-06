//! assistant 正文分帧（对位移植桌面端 `source/decoder.ts` 的纯函数路径）。
//!
//! 规则（spec-generative-ui-runtime §6.1/§6.2）：
//! - 只有顶层（≤3 空格缩进）、info 串匹配 `qaqh-ui v=0.1|0.2 format=tsx|jsonl`
//!   的反引号围栏才有 UI 语义；普通 ```tsx / ```jsonl 仍是代码。
//! - 围栏按 CommonMark 反引号长度配对：关门必须 ≥ 开门长度且不带 info。
//! - 流式期间「可能的围栏头」先缓冲（holdback），不能先绘成 Markdown 再销毁。
//! - 纯函数每次对全文重扫；UI 源码是不可信文本——这里只分帧与提取展示字段
//!   （surfaceId/fallback），不编译不执行。

use std::sync::LazyLock;

use regex::Regex;
use serde_json::Value;

/// 展示字段提取（tsx 源码）——流式期间每帧重扫，静态编译一次。
static RE_SURFACE_ID: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"ui\.surface\(\s*"((?:[^"\\]|\\.)*)""#).unwrap());
static RE_FALLBACK: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"fallback:\s*"((?:[^"\\]|\\.)*)""#).unwrap());

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UiFormat {
    Tsx,
    Jsonl,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ContentPart {
    Markdown(String),
    Ui {
        format: UiFormat,
        /// 围栏体内源码（不含围栏行本身）。
        source: String,
        /// false = 围栏未闭合（流式生成中）；缺结尾永远不得伪造成 seal。
        sealed: bool,
        surface_id: Option<String>,
        fallback: Option<String>,
    },
}

const UI_INFOS: [&str; 4] = [
    "qaqh-ui v=0.1 format=tsx",
    "qaqh-ui v=0.1 format=jsonl",
    "qaqh-ui v=0.2 format=tsx",
    "qaqh-ui v=0.2 format=jsonl",
];

fn ui_format_of(info: &str) -> Option<UiFormat> {
    let tokens: Vec<&str> = info.split_whitespace().collect();
    if tokens.first() != Some(&"qaqh-ui") || tokens.len() > 3 {
        return None;
    }
    let mut format = None;
    for token in &tokens[1..] {
        let (key, value) = token.split_once('=')?;
        match key {
            "v" if value == "0.1" || value == "0.2" => {}
            "format" if value == "tsx" => format = Some(UiFormat::Tsx),
            "format" if value == "jsonl" => format = Some(UiFormat::Jsonl),
            _ => return None,
        }
    }
    format
}

/// 不完整尾行是否仍可能长成 qaqh-ui 围栏头（holdback 判定）：
/// 反引号还没长够 3 个（后面可能续成 ```qaqh-ui），或 info 串为空 /
/// 仍是某个合法 info 的前缀。
fn is_holdback_candidate(line: &str) -> bool {
    if line.chars().count() > 64 {
        return false;
    }
    let body = line.trim_start_matches(' ');
    if line.len() - body.len() > 3 {
        return false;
    }
    let ticks = body.chars().take_while(|c| *c == '`').count();
    let rest = &body[ticks..];
    if rest.contains('`') {
        return false;
    }
    if ticks < 3 {
        return rest.trim().is_empty();
    }
    let info = rest.trim();
    info.is_empty() || UI_INFOS.iter().any(|valid| valid.starts_with(info))
}

/// 从源码提取展示字段；纯正则/逐行 JSON，失败即缺省，不影响分帧。
fn describe(format: UiFormat, source: &str) -> (Option<String>, Option<String>) {
    if format == UiFormat::Tsx {
        let id =
            RE_SURFACE_ID.captures(source).and_then(|c| c.get(1).map(|m| m.as_str().to_string()));
        let fallback = RE_FALLBACK
            .captures(source)
            .and_then(|c| c.get(1).map(|m| m.as_str().to_string()));
        return (id.map(|s| super::unescape(&s)), fallback.map(|s| super::unescape(&s)));
    }
    for raw in source.split('\n').take(50) {
        let line = raw.trim();
        if !line.starts_with('{') {
            continue;
        }
        if let Ok(record) = serde_json::from_str::<Value>(line)
            && record.get("op").and_then(Value::as_str) == Some("surface.open")
        {
            let id = record.get("surface").and_then(Value::as_str).filter(|s| !s.is_empty());
            let fb = record.get("fallback").and_then(Value::as_str).filter(|s| !s.is_empty());
            return (id.map(str::to_string), fb.map(str::to_string));
        }
    }
    (None, None)
}

/// `/^ {0,3}(`{3,})(.*)$/` → (开门长度, info)
fn fence_open(line: &str) -> Option<(usize, &str)> {
    let body = line.trim_start_matches(' ');
    if line.len() - body.len() > 3 {
        return None;
    }
    let ticks = body.chars().take_while(|c| *c == '`').count();
    if ticks < 3 {
        return None;
    }
    Some((ticks, &body[ticks..]))
}

/// `/^ {0,3}(`{3,})\s*$/` → 开门长度（关门不允许 info）
fn fence_close_len(line: &str) -> Option<usize> {
    let body = line.trim_start_matches(' ');
    if line.len() - body.len() > 3 {
        return None;
    }
    let ticks = body.chars().take_while(|c| *c == '`').count();
    if ticks < 3 || !body[ticks..].trim().is_empty() {
        return None;
    }
    Some(ticks)
}

/// 全量分帧。`final_ = true`（流已结束/非流式历史）时不再 holdback——不完整的
/// 尾行按字面处理，不能把用户可见文本吞掉。
pub fn split_content_parts(text: &str, final_: bool) -> Vec<ContentPart> {
    if !text.contains('`') {
        // 快路径与扫描路径同行模型：尾部 \n 的切分伪影不算内容。
        if text.is_empty() {
            return vec![];
        }
        let body = text.strip_suffix('\n').unwrap_or(text);
        return if body.is_empty() {
            vec![]
        } else {
            vec![ContentPart::Markdown(body.to_string())]
        };
    }
    let mut lines: Vec<&str> = text.split('\n').collect();
    if text.ends_with('\n') {
        lines.pop(); // 尾部 \n 的切分伪影不是真实行
    }
    // 流式：末尾不完整行先摘出（holdback 候选判定用）。
    let pending: Option<String> =
        if !final_ && !text.ends_with('\n') { lines.pop().map(str::to_string) } else { None };

    let mut parts: Vec<ContentPart> = vec![];
    let mut md: Vec<String> = vec![];
    let mut ui: Option<(UiFormat, Vec<String>, usize)> = None; // (format, source 行, 开门长度)
    let mut fence_length = 0usize; // 普通围栏开门长度；0 = 不在围栏内

    let flush_md = |md: &mut Vec<String>, parts: &mut Vec<ContentPart>| {
        let joined = md.join("\n");
        if !joined.is_empty() {
            parts.push(ContentPart::Markdown(joined));
        }
        md.clear();
    };

    for line in &lines {
        if let Some((format, source, open_len)) = ui.as_mut() {
            if fence_close_len(line).is_some_and(|len| len >= *open_len) {
                let source_text = source.join("\n");
                let (surface_id, fallback) = describe(*format, &source_text);
                parts.push(ContentPart::Ui {
                    format: *format,
                    source: source_text,
                    sealed: true,
                    surface_id,
                    fallback,
                });
                ui = None;
            } else {
                source.push((*line).to_string());
            }
            continue;
        }
        if fence_length > 0 {
            if fence_close_len(line).is_some_and(|len| len >= fence_length) {
                fence_length = 0;
            }
            md.push((*line).to_string());
            continue;
        }
        if let Some((ticks, info)) = fence_open(line)
            && !info.contains('`')
        {
            if let Some(format) = ui_format_of(info) {
                flush_md(&mut md, &mut parts);
                ui = Some((format, vec![], ticks));
                continue;
            }
            fence_length = ticks;
        }
        md.push((*line).to_string());
    }

    // 收尾：holdback 判定——顶层 md 上、可能长成围栏头的尾行先缓冲（既不当
    // Markdown，也不提前开 UI）。
    let pending = match pending {
        Some(line) if ui.is_none() && is_holdback_candidate(&line) => None,
        other => other,
    };
    if let Some((format, source, open_len)) = ui {
        let sealed = pending
            .as_deref()
            .and_then(fence_close_len)
            .is_some_and(|len| len >= open_len);
        let mut source = source;
        if !sealed
            && let Some(line) = &pending
        {
            source.push(line.clone());
        }
        let source_text = source.join("\n");
        let (surface_id, fallback) = describe(format, &source_text);
        parts.push(ContentPart::Ui { format, source: source_text, sealed, surface_id, fallback });
    } else {
        if let Some(line) = &pending {
            md.push(line.clone());
        }
        flush_md(&mut md, &mut parts);
    }
    parts
}
