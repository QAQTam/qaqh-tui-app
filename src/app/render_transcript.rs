//! transcript 渲染器：TimelineModel → Vec<RenderLine>（预折行，缓存友好）。

use crate::app::render_line::{wrap_text, RenderLine, SpanStyle};
use crate::app::session::SessionState;
use crate::protocol::timeline::{TimelineBlockKind, TimelineToolState, TimelineTurnState};

/// 推理块折叠时保留的尾部行数（历史常量，当前默认展开路径不再截尾，保留供 hide 回退）。
#[allow(dead_code)]
const REASONING_TAIL: usize = 2;
/// 工具输出保留的尾部行数（已由折叠逻辑替代，保留作历史阈值参考）。
/// 单元格截断宽度。
const ARG_PREVIEW: usize = 96;

/// 解析 hunk 头 `@@ -a,b +c,d @@` 取 a,c 起始行
fn parse_hunk_header(header: &str) -> Option<(u32, u32)> {
    // 形如 @@ -1,3 +1,4 @@ 可选 ,b
    let header = header.trim();
    if !header.starts_with("@@") { return None; }
    let inner = header.trim_start_matches('@').trim();
    // 取两段
    let mut parts = inner.split_whitespace();
    let old = parts.next()?;
    let new = parts.next()?;
    let old_num = old.trim_start_matches('-').split(',').next()?.parse::<u32>().ok()?;
    let new_num = new.trim_start_matches('+').split(',').next()?.parse::<u32>().ok()?;
    Some((old_num, new_num))
}
fn fmt_ln(n: u32, w: usize) -> String {
    // 右对齐 w 宽
    format!("{n:>width$}", width = w)
}

// ── Reasoning summary 特判：动词-ing 每句换行 ─────────────────────────────
// summary 来自 OpenAI Responses `response.reasoning_summary_text.delta`，
// 前端收到的是单段无换行的 gerund 句拼接（如 `Gathering ...feature.Synthesizing ...`，
// 句间缺空格/缺换行）；传统 thinking 则已含换行或多段。我们仅对 summary 做句级换行。
fn is_gerund_word(word: &str) -> bool {
    let w = word.trim_matches(|c: char| !c.is_alphabetic());
    if w.len() < 4 {
        return false;
    }
    let lower = w.to_ascii_lowercase();
    lower.ends_with("ing") && lower.chars().all(|c| c.is_ascii_alphabetic())
}

fn sentence_starts_with_gerund(sentence: &str) -> bool {
    let trimmed = sentence.trim_start_matches(|c: char| matches!(c, '•' | '-' | '"' | '\'' | '(' | ' '));
    if let Some(first) = trimmed.split_whitespace().next() {
        let w = first.trim_matches(|c: char| !c.is_alphabetic());
        is_gerund_word(w)
    } else {
        false
    }
}

fn is_cjk(ch: char) -> bool {
    let cp = ch as u32;
    // 简化：命中中日韩统一表意文字区段即可
    (0x4E00..=0x9FFF).contains(&cp)
        || (0x3400..=0x4DBF).contains(&cp)
        || (0x3000..=0x303F).contains(&cp)
        || (0xFF00..=0xFFEF).contains(&cp)
}

fn split_reasoning_sentences(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();
    let chars: Vec<char> = text.chars().collect();
    let n = chars.len();
    let mut i = 0;
    while i < n {
        let c = chars[i];
        current.push(c);
        if c == '.' || c == '。' || c == '!' || c == '！' || c == '?' || c == '？' {
            // 小数点保护：1.2 不拆
            let prev_is_digit = i > 0 && chars[i - 1].is_ascii_digit();
            let next_is_digit = i + 1 < n && chars[i + 1].is_ascii_digit();
            if c == '.' && prev_is_digit && next_is_digit {
                i += 1;
                continue;
            }
            // 寻下一个非空格字符
            let mut j = i + 1;
            while j < n && chars[j].is_whitespace() && chars[j] != '\n' {
                j += 1;
            }
            if j >= n {
                let s = current.trim().to_string();
                if !s.is_empty() {
                    out.push(s);
                }
                current.clear();
            } else {
                let next = chars[j];
                let is_boundary = if c == '.' {
                    next.is_ascii_uppercase()
                } else if c == '。' || c == '！' || c == '？' {
                    true
                } else {
                    next.is_ascii_uppercase() || is_cjk(next)
                };
                // 缩写保护：".a" 小写不算句界
                if is_boundary {
                    let s = current.trim().to_string();
                    if !s.is_empty() {
                        out.push(s);
                    }
                    current.clear();
                    // 跳过句间空白（已入下一句）
                    i = j - 1;
                }
            }
        }
        i += 1;
    }
    if !current.trim().is_empty() {
        out.push(current.trim().to_string());
    }
    out.retain(|s| !s.is_empty());
    out
}

fn looks_like_reasoning_summary(text: &str) -> bool {
    let t = text.trim();
    if t.is_empty() || t.contains('\n') {
        return false;
    }
    // 强信号：缺空格的句界 "feature.Synthesizing"
    let has_missing_space = {
        let ch: Vec<char> = t.chars().collect();
        let mut found = false;
        for idx in 0..ch.len().saturating_sub(1) {
            if ch[idx] == '.' && ch[idx + 1].is_ascii_uppercase() {
                found = true;
                break;
            }
        }
        found
    };
    let sentences = split_reasoning_sentences(t);
    if has_missing_space {
        if sentences.len() >= 2 && sentences.iter().any(|s| sentence_starts_with_gerund(s)) {
            return true;
        }
        return sentences.len() >= 2;
    }
    if sentences.len() < 2 {
        return false;
    }
    // 中文句：含 "。" 且多句即视为 summary
    if t.contains('。') {
        return sentences.len() >= 2;
    }
    let gerund_cnt = sentences.iter().filter(|s| sentence_starts_with_gerund(s)).count();
    gerund_cnt >= 1 && gerund_cnt * 2 >= sentences.len()
}

fn normalize_reasoning_content(text: &str) -> String {
    if looks_like_reasoning_summary(text) {
        return split_reasoning_sentences(text).join("\n");
    }
    if text.contains('\n') {
        // 段内仍可能藏 summary（如单段内拼接），逐段二次判别
        let mut paras: Vec<String> = Vec::new();
        for para in text.split('\n') {
            if para.trim().is_empty() {
                paras.push(String::new());
            } else if looks_like_reasoning_summary(para) {
                paras.extend(split_reasoning_sentences(para));
            } else {
                // 进一步：即便整段不像 summary，也尝试在缺空格场景下修复
                // 只有当 split 后句数>1 且含 gerund 时才替换，避免误伤普通段
                let split = split_reasoning_sentences(para);
                if split.len() >= 2 && split.iter().any(|s| sentence_starts_with_gerund(s)) {
                    paras.extend(split);
                } else {
                    paras.push(para.to_string());
                }
            }
        }
        // 若未发生任何分裂，直接返回原文避免无意义重组
        let joined = paras.join("\n");
        if joined != text {
            return joined;
        }
        return text.to_string();
    }
    text.to_string()
}

#[allow(dead_code)]
pub fn render_transcript(session: &SessionState, width: u16) -> Vec<RenderLine> {
    // 兼容入口：show_reasoning=true 时展示全文（F3 切换由外层 App 控制缓存失效）
    render_transcript_with_opts(session, width, true)
}

pub fn render_transcript_with_opts(session: &SessionState, width: u16, show_reasoning: bool) -> Vec<RenderLine> {
    let width = width.max(20) as usize;
    let mut lines: Vec<RenderLine> = Vec::new();
    let total = session.timeline.turns.len();

    for (turn_idx, turn) in session.timeline.turns.iter().enumerate() {
        // ── 回合分隔 ──
        let state_tag = match turn.state {
            TimelineTurnState::Running => "… running".to_string(),
            TimelineTurnState::Completed => String::new(),
            TimelineTurnState::Failed => turn
                .failure
                .as_ref()
                .map(|f| format!("✗ {}", f.code))
                .unwrap_or_else(|| "✗ failed".into()),
            TimelineTurnState::Cancelled => "⊘ cancelled".into(),
        };
        let num = turn_idx + 1;
        let mut header = RenderLine::new().span("──── ", SpanStyle::Dim);
        header = header.span(format!("turn {num}/{total}"), SpanStyle::Dim);
        if !state_tag.is_empty() {
            let style = match turn.state {
                TimelineTurnState::Failed => SpanStyle::Error,
                TimelineTurnState::Cancelled => SpanStyle::Warn,
                _ => SpanStyle::Dim,
            };
            header = header.span(format!(" · {state_tag}"), style);
        }
        lines.push(header);

        // ── 用户输入 ──
        if !turn.user_text.is_empty() {
            let wrapped = wrap_text(&turn.user_text, width.saturating_sub(2));
            for (i, seg) in wrapped.into_iter().enumerate() {
                let mut line = RenderLine::new();
                line = line.span(if i == 0 { "❯ " } else { "  " }, SpanStyle::Accent);
                line = line.span(seg, SpanStyle::User);
                lines.push(line);
            }
        }

        // ── rounds / blocks ──
        for round in &turn.rounds {
            for block in &round.blocks {
                match block.kind {
                    TimelineBlockKind::Text => push_text_block(&mut lines, &block.text, width, block.is_streaming()),
                    TimelineBlockKind::Reasoning => {
                        push_reasoning_block(&mut lines, &block.text, width, block.is_streaming(), show_reasoning)
                    }
                    TimelineBlockKind::Tool => {
                        if let Some(tool) = &block.tool {
                            let expanded = session.expanded_tools.contains(&tool.tool_call_id);
                            push_tool_card(&mut lines, tool, width, expanded);
                        }
                    }
                    TimelineBlockKind::Notice => {
                        for seg in wrap_text(&block.text, width.saturating_sub(2)) {
                            lines.push(RenderLine::new().span("· ", SpanStyle::Dim).span(seg, SpanStyle::Dim));
                        }
                    }
                }
            }
        }

        // 回合失败详情。
        if let Some(f) = &turn.failure {
            for seg in wrap_text(&format!("{}: {}", f.code, f.message), width.saturating_sub(4)) {
                lines.push(RenderLine::new().span("  ✗ ", SpanStyle::Error).span(seg, SpanStyle::Error));
            }
        }
        lines.push(RenderLine::new());
    }

    if session.timeline.turns.is_empty() {
        lines.push(RenderLine::new().span("（暂无回合——输入消息开始对话）", SpanStyle::Dim));
    }
    if session.timeline.has_more {
        lines.insert(0, RenderLine::new().span("↑ 更早回合已折叠（PgUp 加载）", SpanStyle::Dim));
    }
    lines
}

fn push_text_block(lines: &mut Vec<RenderLine>, text: &str, width: usize, streaming: bool) {
    if streaming {
        // 流式：纯文本低开销，避免半截 markdown 抖动与 syntect 重算
        let shown = format!("{text}▌");
        for seg in wrap_text(&shown, width) {
            lines.push(RenderLine::plain(seg));
        }
        return;
    }
    if crate::app::markdown::is_markdown(text) {
        // 落盘后富化：表格/代码块栅格化，保持单 Paragraph 滚动链路
        let mut md_lines = crate::app::markdown::render_markdown(text, width);
        // 批处理保护：单块超 500 行截断（防 100M 爆存，见 docs/markdown-plan.md）
        if md_lines.len() > 500 {
            let omitted = md_lines.len() - 500;
            md_lines.truncate(500);
            md_lines.push(RenderLine::new().span(format!("  （内容省略 {omitted} 行）"), SpanStyle::Dim));
        }
        lines.extend(md_lines);
        return;
    }
    for seg in wrap_text(text, width) {
        lines.push(RenderLine::plain(seg));
    }
}

fn push_reasoning_block(lines: &mut Vec<RenderLine>, text: &str, width: usize, streaming: bool, show_reasoning: bool) {
    if text.trim().is_empty() {
        return;
    }
    // summary 特判：无换行的动词-ing 句拼接自动逐句换行（eeacd19a 实测句间缺空格/缺换行）
    // normalize 仅在 looks_like_reasoning_summary 为真时注入换行，传统 thinking 保持原样
    let normalized = normalize_reasoning_content(text);
    // opencode ReasoningHeader：流式 Spinner + 折叠标题对齐 `index.tsx:1652`
    // 解析首段作为标题（**Title**\n\nBody 或首行），其余为 body
    let trimmed = normalized.trim();
    let (title, body) = if let Some(stripped) = trimmed.strip_prefix("**") {
        if let Some(end) = stripped.find("**") {
            let t = stripped[..end].trim();
            let b = stripped[end+2..].trim().trim_start_matches('\n').trim();
            (if t.is_empty() { None } else { Some(t.to_owned()) }, b.to_owned())
        } else { (None, trimmed.to_owned()) }
    } else {
        // 取首行作标题（≤48ch）
        let mut parts = trimmed.splitn(2, '\n');
        let first = parts.next().unwrap_or("").trim();
        let rest = parts.next().unwrap_or("").trim();
        if rest.is_empty() { (None, trimmed.to_owned()) }
        else if first.chars().count() <= 48 { (Some(first.to_owned()), rest.to_owned()) }
        else { (None, trimmed.to_owned()) }
    };
    if streaming {
        let frames = ["◐","◑","◒","◓"];
        let ms = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis()).unwrap_or(0);
        let icon = frames[((ms/200) % frames.len() as u128) as usize];
        let header = if let Some(t) = &title { format!("{icon} Thinking: {t}") } else { format!("{icon} Thinking") };
        lines.push(RenderLine::new().span("  ", SpanStyle::Dim).span(header, SpanStyle::Warn));
        if !show_reasoning {
            // hide 模式流式仅保留标题行，不展 body（与 opencode hide 对齐）
            return;
        }
        // 默认展开：流式即全显（8行以上时不截尾，cursor 附末行）
        let wrapped = wrap_text(&body, width.saturating_sub(4));
        for (i, seg) in wrapped.iter().enumerate() {
            let is_last = i == wrapped.len() - 1;
            let shown = if is_last { format!("{seg}▌") } else { seg.clone() };
            lines.push(RenderLine::new().span("    ", SpanStyle::Dim).span(shown, SpanStyle::Reasoning));
        }
        return;
    }
    // 非流式：hide 时单行 `+ Thought: title`（可 F3 展开）
    if !show_reasoning {
        if let Some(t) = title {
            lines.push(RenderLine::new().span("  ", SpanStyle::Dim).span(format!("+ Thought: {t} (F3 展开)"), SpanStyle::Warn));
        } else {
            let preview = body.chars().take(48).collect::<String>();
            lines.push(RenderLine::new().span("  ", SpanStyle::Dim).span(format!("+ Thought: {preview}… (F3 展开)"), SpanStyle::Warn));
        }
        return;
    }
    // 展开态必须保留 Thought 标识（无标题时用通用 Thought，避免裸体正文）
    let header = if let Some(t) = title.as_deref() {
        format!("Thought: {t}")
    } else {
        "Thought".to_string()
    };
    lines.push(RenderLine::new().span("  ", SpanStyle::Dim).span(header, SpanStyle::Warn));
    if body.is_empty() { return; }
    let wrapped = wrap_text(&body, width.saturating_sub(4));
    for seg in wrapped {
        lines.push(RenderLine::new().span("    ", SpanStyle::Dim).span(seg, SpanStyle::Reasoning));
    }
}

/// opencode 式工具图标（对齐 `toolDisplay` 集合） `packages/tui/src/routes/session/index.tsx:2638`
fn tool_icon(name: &str) -> &'static str {
    match name {
        "bash" | "exec" | "shell" | "pwsh" | "powershell" => "$",
        "write" => "←",
        "edit" => "←",
        "glob" => "✱",
        "grep" => "✱",
        "read" => "→",
        "web_fetch" | "webfetch" => "%",
        "web_search" | "websearch" => "◈",
        "apply_patch" => "%",
        "todo" => "⚙",
        "ask" | "question" => "→",
        "skill" => "→",
        "task" => "│",
        _ => "⚙",
    }
}

/// 折叠输出（对齐 `opencode/src/util/collapse-tool-output.ts:1`）
fn collapse_output(output: &str, max_lines: usize, max_chars: usize) -> (String, bool) {
    let lines: Vec<&str> = output.split('\n').collect();
    let char_len = output.chars().count();
    if lines.len() <= max_lines && char_len <= max_chars {
        return (output.to_owned(), false);
    }
    let preview = lines[..max_lines.min(lines.len())].join("\n");
    if preview.chars().count() > max_chars {
        let truncated: String = preview.chars().take(max_chars.saturating_sub(1)).collect();
        return (format!("{truncated}…"), true);
    }
    (format!("{}…", preview), true)
}

/// 从 args_json 提炼可读预览（仅保留 primitives，去除 filePath 重复等）
fn format_args_preview(args_json: &str) -> String {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(args_json) else {
        let one = args_json.replace('\n', " ");
        return if one.chars().count() > ARG_PREVIEW {
            format!("{}…", one.chars().take(ARG_PREVIEW).collect::<String>())
        } else { one };
    };
    if let serde_json::Value::Object(map) = v {
        let mut parts: Vec<String> = Vec::new();
        for (k, val) in map.iter() {
            if matches!(k.as_str(), "filePath" | "path" | "file_path") {
                continue; // 路径由标题单独展示，避免重复
            }
            match val {
                serde_json::Value::String(s) if !s.is_empty() => {
                    let short = if s.chars().count() > 40 { format!("{}…", s.chars().take(40).collect::<String>()) } else { s.clone() };
                    parts.push(format!("{k}={short}"));
                }
                serde_json::Value::Number(_) | serde_json::Value::Bool(_) => parts.push(format!("{k}={val}")),
                _ => {}
            }
            if parts.len() >= 3 { break; }
        }
        if parts.is_empty() {
            return String::new();
        }
        let joined = format!("[{}]", parts.join(", "));
        if joined.chars().count() > ARG_PREVIEW { format!("{}…", joined.chars().take(ARG_PREVIEW).collect::<String>()) } else { joined }
    } else {
        String::new()
    }
}

/// 提炼路径类参数用于标题（read/write/edit 的 filePath / path）
fn extract_path(args_json: Option<&str>) -> Option<String> {
    let s = args_json?;
    let v: serde_json::Value = serde_json::from_str(s).ok()?;
    let obj = v.as_object()?;
    for key in ["filePath", "path", "file_path"] {
        if let Some(serde_json::Value::String(p)) = obj.get(key) { return Some(p.clone()); }
    }
    None
}


fn is_shell_tool(name: &str) -> bool {
    matches!(name, "bash" | "exec" | "shell" | "pwsh" | "powershell")
}

/// 尝试将工具的 `output` JSON 外壳剥离，仅取内部 `output` 字段。
/// 若不是 ExecOutput JSON，则回退为原文；不引入额外错误提示，保持视觉干净。
fn extract_shell_output_text(raw: &str) -> Option<String> {
    let s = raw.trim();
    if !s.starts_with('{') {
        return None;
    }
    let v: serde_json::Value = serde_json::from_str(s).ok()?;
    let obj = v.as_object()?;
    // ExecOutput 形状：{status, command, exit_code, output, truncated, timed_out, cancelled, process_id}
    // backgrounded 时 output 为内层 JSON 字符串，仍以字符串形式透出
    if let Some(out) = obj.get("output").and_then(|x| x.as_str()) {
        return Some(out.to_string());
    }
    // 非字符串 output（如意外对象）则序列化回文本
    if let Some(out) = obj.get("output") {
        if !out.is_null() {
            // 保持可读：若是对象则 pretty-free json
            if out.is_string() {
                return Some(out.as_str().unwrap_or("").to_string());
            } else {
                return Some(out.to_string());
            }
        }
    }
    None
}

fn shell_meta_from_raw(raw: &str) -> Option<(Option<i32>, bool, String)> {
    let v: serde_json::Value = serde_json::from_str(raw.trim()).ok()?;
    let obj = v.as_object()?;
    if !obj.contains_key("output") {
        return None;
    }
    let exit_code = obj.get("exit_code").and_then(|x| x.as_i64()).map(|x| x as i32);
    let truncated = obj.get("truncated").and_then(|x| x.as_bool()).unwrap_or(false);
    let status = obj.get("status").and_then(|x| x.as_str()).unwrap_or("").to_string();
    Some((exit_code, truncated, status))
}

fn push_tool_card(lines: &mut Vec<RenderLine>, tool: &crate::app::timeline_model::ToolCard, width: usize, expanded: bool) {
    // 动画帧：Running 时用四帧转轮（与 opencode Spinner 对齐，200ms/帧，Tick 500ms 驱动重绘）
    let (icon_raw, base_style, is_running) = match tool.state {
        TimelineToolState::Prepared => (tool_icon(&tool.name), SpanStyle::Dim, false),
        TimelineToolState::Running => {
            let frames = ["◐", "◑", "◒", "◓"];
            let ms = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis())
                .unwrap_or(0);
            let idx = ((ms / 200) % frames.len() as u128) as usize;
            (frames[idx], SpanStyle::ToolRun, true)
        }
        TimelineToolState::Succeeded => ("●", SpanStyle::ToolOk, false),
        TimelineToolState::Failed => ("✗", SpanStyle::ToolFail, false),
    };
    // 权限覆盖色（对齐 opencode InlineTool fg: warning）
    let (icon, style) = if tool.permission.is_some() {
        (icon_raw, SpanStyle::Warn)
    } else if tool.failure.is_some() && tool.state == TimelineToolState::Failed {
        (icon_raw, SpanStyle::ToolFail)
    } else { (icon_raw, base_style) };

    // ── 标题行：InlineTool 形态（单行 icon + name + 路径/摘要） `opencode InlineToolRow:1967`
    let path = extract_path(tool.args_json.as_deref());
    let header_extra = if let Some(p) = &path {
        let short = crate::app::truncate_str(p, 36);
        format!(" {short}")
    } else if let Some(summary) = tool.summary.as_deref().filter(|s| !s.is_empty()) {
        let one = summary.replace('\n', " ").chars().take(48).collect::<String>();
        format!(" {one}")
    } else { String::new() };

    // Block 判定：含 diff / 长输出 / 诊断即用 BlockTool 左线
    let has_diff = tool.diff.as_deref().is_some_and(|d| !d.trim().is_empty());
    let output_len = tool.output.as_deref().map(|s| s.lines().count()).unwrap_or(0) + tool.progress.lines().count();
    let is_block = has_diff || output_len > 4 || tool.state == TimelineToolState::Running && !tool.progress.is_empty();

    if is_block {
        // BlockTool 标题：`# name path` 灰底左线（复刻 `BlockTool 1995 border left ┃ bg panel`）
        let title = if let Some(p) = path {
            let display = crate::app::truncate_str(&p, width.saturating_sub(10));
            format!("# {} {display}", tool.name)
        } else {
            format!("# {}", tool.name)
        };
        // 左线用 ┃（SplitBorder vertical）
        lines.push(
            RenderLine::new()
                .span(" ┃ ", SpanStyle::Dim)
                .span(title, SpanStyle::Dim),
        );
        // 状态行：icon + 状态 + 动画尾
        let state_label = match tool.state {
            TimelineToolState::Running => " running",
            TimelineToolState::Succeeded => " completed",
            TimelineToolState::Failed => " failed",
            TimelineToolState::Prepared => " prepared",
        };
        lines.push(
            RenderLine::new()
                .span(" ┃ ", SpanStyle::Dim)
                .span(format!("{icon} "), style)
                .span(tool.name.clone(), SpanStyle::Accent)
                .span(state_label, style)
                .span(if is_running { " ⋯" } else { "" }, SpanStyle::Dim),
        );
    } else {
        // InlineTool 单行
        let state_suffix = match tool.state {
            TimelineToolState::Running => " ⋯",
            _ => "",
        };
        let mut header = RenderLine::new()
            .span("  ", SpanStyle::Dim)
            .span(format!("{icon} "), style)
            .span(tool.name.clone(), if tool.permission.is_some() { SpanStyle::Warn } else { SpanStyle::Accent });
        if !header_extra.trim().is_empty() {
            header = header.span(header_extra.clone(), if tool.state == TimelineToolState::Succeeded { SpanStyle::Dim } else { SpanStyle::Plain });
        }
        header = header.span(state_suffix, SpanStyle::Dim);
        // 权限/失败的额外内联提示
        if tool.permission.is_some() {
            header = header.span(" ⚠ 需授权", SpanStyle::Warn);
        } else if let Some(err) = &tool.failure {
            let hint = crate::app::truncate_str(&err.message, 28);
            header = header.span(format!(" ✗ {hint}"), SpanStyle::ToolFail);
        }
        lines.push(header);
        // Inline 下若无 Block，不再展开 diff/输出，返回
        if !has_diff && output_len == 0 && tool.args_json.is_none() {
            return;
        }
    }

    // ── 参预览（非路径部分）─ 对齐 opencode `input()` 过滤
    if let Some(args) = tool.args_json.as_deref().filter(|s| !s.is_empty() && *s != "{}") {
        let preview = format_args_preview(args);
        if !preview.is_empty() {
            let one_line = preview.replace('\n', " ");
            for seg in wrap_text(&one_line, width.saturating_sub(6)) {
                let prefix = if is_block { " ┃ ⌗ " } else { "    ⌗ " };
                lines.push(RenderLine::new().span(prefix, SpanStyle::Dim).span(seg, SpanStyle::Dim));
            }
        }
    }

    // ── Diff 块：行级着色 + 自适应 split/unified + 行号 gutter（opencode 2401/2595）
    if let Some(diff) = &tool.diff {
        let added = diff.lines().filter(|l| l.starts_with('+') && !l.starts_with("+++")).count();
        let removed = diff.lines().filter(|l| l.starts_with('-') && !l.starts_with("---")).count();
        let prefix = if is_block { " ┃ Δ " } else { "    Δ " };
        lines.push(
            RenderLine::new()
                .span(prefix, SpanStyle::Dim)
                .span(format!("+{added}"), SpanStyle::DiffAdd)
                .span(" ", SpanStyle::Dim)
                .span(format!("−{removed}"), SpanStyle::DiffDel)
                .span(if width > 120 && is_block { "  (split)" } else { "" }, SpanStyle::Dim),
        );
        if width > 120 && is_block {
            // split 双栏：左旧/右新 各含 3宽行号 + 内容
            let inner = width.saturating_sub(7);
            let left_w = inner / 2;
            let right_w = inner - left_w;
            let ln_w = 3usize; // 行号宽
            let left_content_w = left_w.saturating_sub(ln_w + 1);
            let right_content_w = right_w.saturating_sub(ln_w + 1);
            let mut pending_removed: Vec<(String, u32)> = Vec::new();
            let mut pending_added: Vec<(String, u32)> = Vec::new();
            let mut shown = 0usize;
            let mut old_ln: u32 = 1;
            let mut new_ln: u32 = 1;
            for raw in diff.lines().take(80) {
                if raw.starts_with("---") || raw.starts_with("+++") {
                    // 刷 pending
                    {
                        let max = pending_removed.len().max(pending_added.len());
                        for i in 0..max {
                            if shown >= 60 { break; }
                            let (l_txt, l_no) = pending_removed.get(i).map(|(s, n)| (crate::app::truncate_str(s, left_content_w), *n)).unwrap_or((String::new(), 0));
                            let (r_txt, r_no) = pending_added.get(i).map(|(s, n)| (crate::app::truncate_str(s, right_content_w), *n)).unwrap_or((String::new(), 0));
                            let l_num = if l_no != 0 { fmt_ln(l_no, ln_w) } else { "   ".into() };
                            let r_num = if r_no != 0 { fmt_ln(r_no, ln_w) } else { "   ".into() };
                            lines.push(
                                RenderLine::new()
                                    .span(" ┃ ", SpanStyle::Dim)
                                    .span(l_num, SpanStyle::Dim).span(" ", SpanStyle::Dim)
                                    .span(format!("{:<width$}", l_txt, width = left_content_w), if pending_removed.get(i).is_some() { SpanStyle::DiffDel } else { SpanStyle::Dim })
                                    .span(" │ ", SpanStyle::Dim)
                                    .span(r_num, SpanStyle::Dim).span(" ", SpanStyle::Dim)
                                    .span(r_txt, if pending_added.get(i).is_some() { SpanStyle::DiffAdd } else { SpanStyle::Dim }),
                            );
                            shown += 1;
                        }
                        pending_removed.clear();
                        pending_added.clear();
                    }
                    lines.push(RenderLine::new().span(" ┃ ", SpanStyle::Dim).span(raw.to_owned(), SpanStyle::Dim));
                } else if raw.starts_with("@@") {
                    if let Some((o, n)) = parse_hunk_header(raw) { old_ln = o; new_ln = n; }
                    {
                        let max = pending_removed.len().max(pending_added.len());
                        for i in 0..max {
                            if shown >= 60 { break; }
                            let (l_txt, l_no) = pending_removed.get(i).map(|(s, n)| (crate::app::truncate_str(s, left_content_w), *n)).unwrap_or((String::new(), 0));
                            let (r_txt, r_no) = pending_added.get(i).map(|(s, n)| (crate::app::truncate_str(s, right_content_w), *n)).unwrap_or((String::new(), 0));
                            let l_num = if l_no != 0 { fmt_ln(l_no, ln_w) } else { "   ".into() };
                            let r_num = if r_no != 0 { fmt_ln(r_no, ln_w) } else { "   ".into() };
                            lines.push(
                                RenderLine::new()
                                    .span(" ┃ ", SpanStyle::Dim)
                                    .span(l_num, SpanStyle::Dim).span(" ", SpanStyle::Dim)
                                    .span(format!("{:<width$}", l_txt, width = left_content_w), if pending_removed.get(i).is_some() { SpanStyle::DiffDel } else { SpanStyle::Dim })
                                    .span(" │ ", SpanStyle::Dim)
                                    .span(r_num, SpanStyle::Dim).span(" ", SpanStyle::Dim)
                                    .span(r_txt, if pending_added.get(i).is_some() { SpanStyle::DiffAdd } else { SpanStyle::Dim }),
                            );
                            shown += 1;
                        }
                        pending_removed.clear();
                        pending_added.clear();
                    }
                    lines.push(RenderLine::new().span(" ┃ ", SpanStyle::Dim).span(raw.to_owned(), SpanStyle::Dim));
                } else if raw.starts_with('-') {
                    pending_removed.push((raw[1..].to_owned(), old_ln));
                    old_ln += 1;
                } else if raw.starts_with('+') {
                    pending_added.push((raw[1..].to_owned(), new_ln));
                    new_ln += 1;
                } else {
                    {
                        let max = pending_removed.len().max(pending_added.len());
                        for i in 0..max {
                            if shown >= 60 { break; }
                            let (l_txt, l_no) = pending_removed.get(i).map(|(s, n)| (crate::app::truncate_str(s, left_content_w), *n)).unwrap_or((String::new(), 0));
                            let (r_txt, r_no) = pending_added.get(i).map(|(s, n)| (crate::app::truncate_str(s, right_content_w), *n)).unwrap_or((String::new(), 0));
                            let l_num = if l_no != 0 { fmt_ln(l_no, ln_w) } else { "   ".into() };
                            let r_num = if r_no != 0 { fmt_ln(r_no, ln_w) } else { "   ".into() };
                            lines.push(
                                RenderLine::new()
                                    .span(" ┃ ", SpanStyle::Dim)
                                    .span(l_num, SpanStyle::Dim).span(" ", SpanStyle::Dim)
                                    .span(format!("{:<width$}", l_txt, width = left_content_w), if pending_removed.get(i).is_some() { SpanStyle::DiffDel } else { SpanStyle::Dim })
                                    .span(" │ ", SpanStyle::Dim)
                                    .span(r_num, SpanStyle::Dim).span(" ", SpanStyle::Dim)
                                    .span(r_txt, if pending_added.get(i).is_some() { SpanStyle::DiffAdd } else { SpanStyle::Dim }),
                            );
                            shown += 1;
                        }
                        pending_removed.clear();
                        pending_added.clear();
                    }
                    if raw.trim().is_empty() { old_ln += 1; new_ln += 1; continue; }
                    let txt = raw.strip_prefix(' ').unwrap_or(raw);
                    let l_no = old_ln; let r_no = new_ln;
                    old_ln += 1; new_ln += 1;
                    let l = crate::app::truncate_str(txt, left_content_w);
                    let r = crate::app::truncate_str(txt, right_content_w);
                    lines.push(
                        RenderLine::new()
                            .span(" ┃ ", SpanStyle::Dim)
                            .span(fmt_ln(l_no, ln_w), SpanStyle::Dim).span(" ", SpanStyle::Dim)
                            .span(format!("{:<width$}", l, width = left_content_w), SpanStyle::Dim)
                            .span(" │ ", SpanStyle::Dim)
                            .span(fmt_ln(r_no, ln_w), SpanStyle::Dim).span(" ", SpanStyle::Dim)
                            .span(r, SpanStyle::Dim),
                    );
                    shown += 1;
                    if shown >= 60 { break; }
                }
                if shown >= 60 { break; }
            }
            {
                let max = pending_removed.len().max(pending_added.len());
                for i in 0..max {
                    if shown >= 60 { break; }
                    let (l_txt, l_no) = pending_removed.get(i).map(|(s, n)| (crate::app::truncate_str(s, left_content_w), *n)).unwrap_or((String::new(), 0));
                    let (r_txt, r_no) = pending_added.get(i).map(|(s, n)| (crate::app::truncate_str(s, right_content_w), *n)).unwrap_or((String::new(), 0));
                    let l_num = if l_no != 0 { fmt_ln(l_no, ln_w) } else { "   ".into() };
                    let r_num = if r_no != 0 { fmt_ln(r_no, ln_w) } else { "   ".into() };
                    lines.push(
                        RenderLine::new()
                            .span(" ┃ ", SpanStyle::Dim)
                            .span(l_num, SpanStyle::Dim).span(" ", SpanStyle::Dim)
                            .span(format!("{:<width$}", l_txt, width = left_content_w), if pending_removed.get(i).is_some() { SpanStyle::DiffDel } else { SpanStyle::Dim })
                            .span(" │ ", SpanStyle::Dim)
                            .span(r_num, SpanStyle::Dim).span(" ", SpanStyle::Dim)
                            .span(r_txt, if pending_added.get(i).is_some() { SpanStyle::DiffAdd } else { SpanStyle::Dim }),
                    );
                    shown += 1;
                }
            }
            if diff.lines().count() > 80 {
                lines.push(RenderLine::new().span(format!(" ┃   … {} 行未展示", diff.lines().count() - 80), SpanStyle::Dim));
            }
        } else {
            // unified + 行号 gutter 3宽
            let mut shown = 0usize;
            let mut old_ln: u32 = 1;
            let mut new_ln: u32 = 1;
            for raw in diff.lines().take(80) {
                if raw.starts_with("---") || raw.starts_with("+++") {
                    lines.push(RenderLine::new().span(if is_block { " ┃ " } else { "    " }, SpanStyle::Dim).span(raw.to_owned(), SpanStyle::Dim));
                } else if raw.starts_with("@@") {
                    if let Some((o, n)) = parse_hunk_header(raw) { old_ln = o; new_ln = n; }
                    lines.push(RenderLine::new().span(if is_block { " ┃ " } else { "    " }, SpanStyle::Dim).span(raw.to_owned(), SpanStyle::Dim));
                } else if raw.starts_with('+') {
                    let ln = fmt_ln(new_ln, 3);
                    new_ln += 1;
                    let txt = &raw[1..];
                    let seg = crate::app::truncate_str(txt, width.saturating_sub(10));
                    lines.push(RenderLine::new().span(if is_block { " ┃ " } else { "    " }, SpanStyle::Dim).span(ln, SpanStyle::Dim).span(" +", SpanStyle::DiffAdd).span(seg, SpanStyle::DiffAdd));
                } else if raw.starts_with('-') {
                    let ln = fmt_ln(old_ln, 3);
                    old_ln += 1;
                    let txt = &raw[1..];
                    let seg = crate::app::truncate_str(txt, width.saturating_sub(10));
                    lines.push(RenderLine::new().span(if is_block { " ┃ " } else { "    " }, SpanStyle::Dim).span(ln, SpanStyle::Dim).span(" -", SpanStyle::DiffDel).span(seg, SpanStyle::DiffDel));
                } else if !raw.trim().is_empty() {
                    let ln = fmt_ln(new_ln, 3);
                    old_ln += 1; new_ln += 1;
                    let txt = raw.strip_prefix(' ').unwrap_or(raw);
                    let seg = crate::app::truncate_str(txt, width.saturating_sub(10));
                    lines.push(RenderLine::new().span(if is_block { " ┃ " } else { "    " }, SpanStyle::Dim).span(ln, SpanStyle::Dim).span("  ", SpanStyle::Dim).span(seg, SpanStyle::Dim));
                } else {
                    old_ln += 1; new_ln += 1;
                }
                shown += 1;
                if shown >= 60 { break; }
            }
            if diff.lines().count() > 80 {
                lines.push(RenderLine::new().span(format!("{}   … {} 行未展示", if is_block { " ┃ " } else { "    " }, diff.lines().count() - 80), SpanStyle::Dim));
            }
        }
    }

    // ── 输出：shell 8 行流动 + JSON 剥壳，视觉一致无 [stderr] 前缀 ──
    if is_shell_tool(&tool.name) {
        let raw_output = tool.output.as_deref().unwrap_or("");
        let unwrapped = extract_shell_output_text(raw_output);
        let shell_meta = shell_meta_from_raw(raw_output);
        let src = if tool.state == TimelineToolState::Running {
            if !tool.progress.trim().is_empty() {
                tool.progress.clone()
            } else {
                unwrapped.clone().unwrap_or_default()
            }
        } else if let Some(ref inner) = unwrapped {
            if !inner.trim().is_empty() {
                inner.clone()
            } else if !tool.progress.trim().is_empty() {
                tool.progress.clone()
            } else {
                String::new()
            }
        } else if !tool.progress.trim().is_empty() {
            tool.progress.clone()
        } else if !raw_output.trim().is_empty() {
            raw_output.to_string()
        } else {
            String::new()
        };
        if !src.trim().is_empty() {
            let max_lines = 8usize;
            let expanded_limit = 24usize;
            let total_raw_lines = src.lines().count();
            let max_chars = max_lines * width.saturating_sub(6).max(20);
            let needs_collapse = total_raw_lines > max_lines || src.chars().count() > max_chars;
            let display_text = if tool.state == TimelineToolState::Running {
                if total_raw_lines > max_lines {
                    src.lines().skip(total_raw_lines - max_lines).collect::<Vec<_>>().join("\n")
                } else {
                    src.clone()
                }
            } else if needs_collapse && !expanded {
                if total_raw_lines > max_lines {
                    src.lines().skip(total_raw_lines - max_lines).collect::<Vec<_>>().join("\n")
                } else {
                    let mut truncated: String = src.chars().take(max_chars.saturating_sub(1)).collect();
                    truncated.push('…');
                    truncated
                }
            } else if src.lines().count() > expanded_limit && !expanded {
                src.lines().take(expanded_limit).collect::<Vec<_>>().join("\n")
            } else {
                src.clone()
            };
            let overflow = needs_collapse;
            let line_prefix = if is_block { " ┃ │ " } else { "    │ " };
            let mut shown_lines = 0usize;
            for out in display_text.lines() {
                if out.is_empty() {
                    lines.push(RenderLine::new().span(line_prefix, SpanStyle::Dim).span("", SpanStyle::Dim));
                    shown_lines += 1;
                    continue;
                }
                for seg in wrap_text(out, width.saturating_sub(6)) {
                    lines.push(RenderLine::new().span(line_prefix, SpanStyle::Dim).span(seg, SpanStyle::Dim));
                    shown_lines += 1;
                    if shown_lines >= expanded_limit { break; }
                }
                if shown_lines >= expanded_limit { break; }
            }
            if overflow {
                let mut hint_text = if expanded { "F7 收起".to_string() } else { "F7 展开".to_string() };
                if !is_running {
                    if let Some((exit, truncated, _)) = shell_meta {
                        if let Some(code) = exit {
                            if code != 0 { hint_text.push_str(&format!(" · exit {code}")); }
                        }
                        if truncated { hint_text.push_str(" · 截断"); }
                    }
                }
                lines.push(RenderLine::new().span(format!("{}  ", line_prefix), SpanStyle::Dim).span(hint_text, SpanStyle::Dim));
            }
            if is_running {
                if let Some(last) = lines.last_mut() {
                    if let Some(span) = last.spans.last_mut() { span.text.push('▌'); }
                }
            }
        }
    } else {
        let mut combined = String::new();
        if let Some(output) = tool.output.as_deref().filter(|s| !s.is_empty()) {
            combined.push_str(output);
            if !tool.progress.is_empty() { combined.push('\n'); }
        }
        combined.push_str(&tool.progress);
        if !combined.trim().is_empty() {
            let max_lines = 4usize;
            let max_chars = max_lines * width.saturating_sub(6).max(20);
            let (shown_text, overflow) = collapse_output(&combined, max_lines, max_chars);
            let display = if overflow && !expanded { shown_text } else { combined };
            let line_prefix = if is_block { " ┃ │ " } else { "    │ " };
            let mut shown_lines = 0usize;
            for out in display.lines().take(if overflow && !expanded { max_lines } else { 24 }) {
                for seg in wrap_text(out, width.saturating_sub(6)) {
                    lines.push(RenderLine::new().span(line_prefix, SpanStyle::Dim).span(seg, SpanStyle::Dim));
                    shown_lines += 1;
                    if shown_lines > 24 { break; }
                }
            }
            if overflow {
                let hint = if expanded { "F7 收起" } else { "F7 展开" };
                lines.push(RenderLine::new().span(format!("{}  ", line_prefix), SpanStyle::Dim).span(hint, SpanStyle::Dim));
            }
            if is_running && !overflow {
                if let Some(last) = lines.last_mut() {
                    if let Some(span) = last.spans.last_mut() { span.text.push('▌'); }
                }
            }
        }
    }

    if let Some(err) = &tool.failure {
        for seg in wrap_text(&format!("{}: {}", err.code, err.message), width.saturating_sub(6)) {
            let pfx = if is_block { " ┃ ✗ " } else { "    ✗ " };
            lines.push(RenderLine::new().span(pfx, SpanStyle::Error).span(seg, SpanStyle::Error));
        }
    }
    if let Some(perm) = &tool.permission {
        let pfx = if is_block { " ┃ ⚠ " } else { "    ⚠ " };
        lines.push(
            RenderLine::new()
                .span(pfx, SpanStyle::Warn)
                .span(format!("等待权限：{}（risk {}）", perm.category, perm.risk), SpanStyle::Warn),
        );
    }
    // Block 底部收口留白（对齐 BlockTool paddingBottom 1）
    if is_block {
        lines.push(RenderLine::new().span(" ┃", SpanStyle::Dim));
    }
}

/// 会话信息行（标签栏下方）。
pub fn render_session_info(session: &SessionState, width: u16) -> Vec<RenderLine> {
    let mut spans: Vec<(String, SpanStyle)> = Vec::new();
    if let Some(model) = session.display_model() {
        spans.push((model, SpanStyle::Accent));
    }
    match session.mode {
        crate::protocol::command::ConversationMode::Plan => {
            spans.push(("plan".into(), SpanStyle::Warn));
        }
        crate::protocol::command::ConversationMode::Code => {
            spans.push(("code".into(), SpanStyle::Dim));
        }
    }
    if session.code_added > 0 || session.code_removed > 0 {
        spans.push((format!("+{}", session.code_added), SpanStyle::DiffAdd));
        spans.push((format!("−{}", session.code_removed), SpanStyle::DiffDel));
    }
    if let Some(compact) = &session.compact_status {
        spans.push((format!("compact:{compact}"), SpanStyle::Dim));
    }
    if let Some(err) = &session.last_error {
        spans.push((format!("err:{}", err.code), SpanStyle::Error));
    }
    let seed_label = format!("#{}", session.seed);
    let mut line = RenderLine::new().span(" ", SpanStyle::Dim);
    let mut used = 2usize;
    for (text, style) in spans {
        let w = text.chars().count() + 2;
        if used + w > width as usize {
            break;
        }
        line = line.span(format!("{text}  "), style);
        used += w;
    }
    // 右侧 seed。
    let seed_w = seed_label.chars().count() + 1;
    if used + seed_w <= width as usize {
        let pad = width as usize - used - seed_w;
        line = line.span(" ".repeat(pad), SpanStyle::Dim);
        line = line.span(seed_label, SpanStyle::Dim);
    }
    vec![line]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::timeline_model::{Block, Round, TimelineModel, Turn, ToolCard};
    use crate::protocol::timeline::{TimelineBlockKind, TimelineBlockState, TimelineToolState, TimelineTurnState};

    #[test]
    fn collapse_output_truncates_by_lines_and_chars() {
        let out = "a\nb\nc\nd\ne";
        let (shown, overflow) = collapse_output(out, 3, 100);
        assert!(overflow);
        assert_eq!(shown.lines().count(), 3);
        let (shown2, overflow2) = collapse_output("short", 3, 100);
        assert!(!overflow2);
        assert_eq!(shown2, "short");
    }

    #[test]
    fn hunk_header_parsing() {
        assert_eq!(parse_hunk_header("@@ -1,3 +1,4 @@"), Some((1, 1)));
        assert_eq!(parse_hunk_header("@@ -10 +20 @@"), Some((10, 20)));
        assert_eq!(parse_hunk_header("@@ -0,0 +1 @@"), Some((0, 1)));
        assert!(parse_hunk_header("--- a/file").is_none());
    }

    #[test]
    fn tool_card_renders_inline_and_block() {
        let tool_inline = ToolCard {
            tool_call_id: "c1".into(),
            name: "read".into(),
            state: TimelineToolState::Succeeded,
            summary: Some("ok".into()),
            args_json: Some(r#"{"filePath":"src/lib.rs"}"#.into()),
            output: Some("content".into()),
            diff: None,
            progress: String::new(),
            failure: None,
            permission: None,
        };
        let mut lines = Vec::new();
        push_tool_card(&mut lines, &tool_inline, 80, false);
        assert!(lines.iter().any(|l| l.spans.iter().any(|s| s.text.contains("read"))));

        let diff = "--- a/src/lib.rs\n+++ b/src/lib.rs\n@@ -1,2 +1,2 @@\n-old\n+new\n";
        let tool_block = ToolCard {
            tool_call_id: "c2".into(),
            name: "edit".into(),
            state: TimelineToolState::Succeeded,
            summary: None,
            args_json: Some(r#"{"filePath":"src/lib.rs"}"#.into()),
            output: Some("ok".into()),
            diff: Some(diff.into()),
            progress: String::new(),
            failure: None,
            permission: None,
        };
        let mut lines2 = Vec::new();
        push_tool_card(&mut lines2, &tool_block, 130, false); // wide -> split
        assert!(lines2.iter().any(|l| l.spans.iter().any(|s| s.text.contains("Δ"))));
        assert!(lines2.iter().any(|l| l.spans.iter().any(|s| s.text.contains("split"))));
        let mut lines3 = Vec::new();
        push_tool_card(&mut lines3, &tool_block, 80, false); // narrow -> unified
        assert!(lines3.iter().any(|l| l.spans.iter().any(|s| s.text.contains("Δ"))));
    }

    #[test]
    fn rendering_respects_show_reasoning() {
        let mut model = TimelineModel::default();
        let block = Block {
            block_id: "b1".into(),
            block_order: 0,
            kind: TimelineBlockKind::Reasoning,
            state: TimelineBlockState::Sealed,
            text: "**Title**\n\nBody content here\nsecond line".into(),
            tool: None,
            last_fragment: 0,
        };
        let round = Round { round_num: 0, sealed: true, is_final: true, blocks: vec![block] };
        let turn = Turn { turn_id: "t1".into(), user_text: "hi".into(), state: TimelineTurnState::Completed, failure: None, rounds: vec![round] };
        let mut sess = crate::app::session::SessionState::new("s".into());
        sess.timeline.turns.push(turn);
        sess.timeline.version = 1;
        let lines_hide = render_transcript_with_opts(&sess, 80, false);
        let lines_show = render_transcript_with_opts(&sess, 80, true);
        // hide 应折叠为单行 + 提示
        assert!(lines_hide.iter().any(|l| l.spans.iter().any(|s| s.text.contains("F3"))));
        assert!(lines_show.iter().any(|l| l.spans.iter().any(|s| s.text.contains("Body"))));
    }

    #[test]
    fn reasoning_summary_split_gerund() {
        let text = "Gathering project structure, git state, and key modules to summarize the Rust TUI architecture and ongoing markdown feature.Synthesizing the exploration into a Chinese summary with architecture layers, PLAN.md divergence, git history, and risks.";
        let normalized = normalize_reasoning_content(text);
        assert!(normalized.contains('\n'), "summary 应被注入换行");
        let parts: Vec<&str> = normalized.split('\n').collect();
        assert_eq!(parts.len(), 2);
        assert!(parts[0].starts_with("Gathering"));
        assert!(parts[1].starts_with("Synthesizing"));
        // 渲染后应产生多行 Reasoning
        let mut sess = crate::app::session::SessionState::new("s".into());
        let block = Block { block_id: "b1".into(), block_order: 0, kind: TimelineBlockKind::Reasoning, state: TimelineBlockState::Sealed, text: text.to_string(), tool: None, last_fragment: 0 };
        sess.timeline.turns.push(Turn { turn_id: "t1".into(), user_text: "".into(), state: TimelineTurnState::Completed, failure: None, rounds: vec![Round { round_num: 0, sealed: true, is_final: true, blocks: vec![block]}]});
        let lines = render_transcript_with_opts(&sess, 120, true);
        // 至少 Thought 标题 + 2 行 body
        let reasoning_lines = lines.iter().filter(|l| l.spans.iter().any(|s| s.text.contains("Gathering") || s.text.contains("Synthesizing"))).count();
        assert!(reasoning_lines >= 2);
    }

    #[test]
    fn reasoning_traditional_not_split() {
        let text = "This is a normal paragraph with Reasoning content. It should not be split because not gerund.";
        assert!(!looks_like_reasoning_summary(text));
        assert_eq!(normalize_reasoning_content(text), text);
    }

    #[test]
    fn reasoning_summary_with_space_also_split() {
        let text = "Reviewing collected project files and planning a systematic bash-based read to complete the exploration. Batching bash reads to collect remaining protocol files.";
        assert!(looks_like_reasoning_summary(text));
        let n = normalize_reasoning_content(text);
        assert_eq!(n.split('\n').count(), 2);
    }

    #[test]
    fn reasoning_single_sentence_no_split() {
        let text = "Synthesizing gathered file and git data to summarize architecture, tech stack, and uncommitted changes.";
        assert!(!looks_like_reasoning_summary(text));
    }

    #[test]
    fn reasoning_decimal_protection() {
        let text = "Updating version to 1.2 for release. Checking tests.";
        // 虽含小数点但仍是两句，且 Checking 为 gerund -> 视为 summary，允许分裂
        // 关键是 1.2 不被误拆为两句
        let parts = split_reasoning_sentences(text);
        assert_eq!(parts.len(), 2);
        assert!(parts[0].contains("1.2"));
    }

    #[test]
    fn reasoning_chinese_sentences() {
        let text = "分析架构。评估方案。设计菜单。";
        let parts = split_reasoning_sentences(text);
        assert_eq!(parts.len(), 3);
        assert!(looks_like_reasoning_summary(text));
    }

    #[test]
    fn shell_output_unwrap_and_streaming_slice() {
        let raw = r#"{"status":"completed","command":"bash ...","exit_code":0,"output":"line1\nline2\nline3","truncated":false,"timed_out":false,"cancelled":false}"#;
        let inner = extract_shell_output_text(raw).unwrap();
        assert_eq!(inner, "line1\nline2\nline3");
        // streaming 8 行尾
        let mut long = String::new();
        for i in 1..=20 {
            long.push_str(&format!("line{i}\n"));
        }
        let tool = ToolCard {
            tool_call_id: "c1".into(),
            name: "bash".into(),
            state: TimelineToolState::Running,
            summary: None,
            args_json: None,
            output: Some(raw.to_string()),
            diff: None,
            progress: long.clone(),
            failure: None,
            permission: None,
        };
        let mut lines = Vec::new();
        push_tool_card(&mut lines, &tool, 80, false);
        // 应包含尾行 line20，且不含 JSON 外壳
        assert!(lines.iter().any(|l| l.spans.iter().any(|s| s.text.contains("line20"))));
        assert!(!lines.iter().any(|l| l.spans.iter().any(|s| s.text.contains("\"command\""))));
        assert!(lines.iter().any(|l| l.spans.iter().any(|s| s.text.contains("▌"))));
    }

    #[test]
    fn shell_done_shows_unwrapped_tail_and_no_stderr_prefix() {
        let raw = r#"{"status":"completed","command":"bash ...","exit_code":1,"output":"out line\nerr line","truncated":false,"timed_out":false,"cancelled":false}"#;
        let tool = ToolCard {
            tool_call_id: "c2".into(),
            name: "bash".into(),
            state: TimelineToolState::Failed,
            summary: None,
            args_json: None,
            output: Some(raw.to_string()),
            diff: None,
            progress: String::new(),
            failure: None,
            permission: None,
        };
        let mut lines = Vec::new();
        push_tool_card(&mut lines, &tool, 80, false);
        let flat: String = lines.iter().flat_map(|l| l.spans.iter().map(|s| s.text.clone())).collect::<Vec<_>>().join("\n");
        assert!(flat.contains("out line"));
        assert!(flat.contains("err line"));
        assert!(!flat.contains("[stderr]"));
        assert!(!flat.contains("\"status\""));
    }

    #[test]
    fn reasoning_streaming_full_expand_by_default() {
        let mut sess = crate::app::session::SessionState::new("s".into());
        let text = (1..=8).map(|i| format!("line {i}")).collect::<Vec<_>>().join("\n");
        let block = Block { block_id: "b1".into(), block_order: 0, kind: TimelineBlockKind::Reasoning, state: TimelineBlockState::Open, text: text.clone(), tool: None, last_fragment: 0 };
        sess.timeline.turns.push(Turn { turn_id: "t1".into(), user_text: "".into(), state: TimelineTurnState::Running, failure: None, rounds: vec![Round { round_num: 0, sealed: false, is_final: false, blocks: vec![block]}]});
        let lines = render_transcript_with_opts(&sess, 120, true);
        let reasoning_cnt = lines.iter().filter(|l| l.spans.iter().any(|s| s.text.contains("line"))).count();
        assert!(reasoning_cnt >= 8, "streaming 默认全显 {reasoning_cnt}");
    }
}
