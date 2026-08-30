//! transcript 渲染器：TimelineModel → Vec<RenderLine>（预折行，缓存友好）。

use crate::app::render_line::{wrap_text, RenderLine, SpanStyle};
use crate::app::session::SessionState;
use crate::protocol::timeline::{TimelineBlockKind, TimelineToolState, TimelineTurnState};

/// 推理块折叠时保留的尾部行数。
const REASONING_TAIL: usize = 2;
/// 工具输出保留的尾部行数。
const OUTPUT_TAIL: usize = 3;
/// 单元格截断宽度。
const ARG_PREVIEW: usize = 96;

pub fn render_transcript(session: &SessionState, width: u16) -> Vec<RenderLine> {
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
                        push_reasoning_block(&mut lines, &block.text, width, block.is_streaming())
                    }
                    TimelineBlockKind::Tool => {
                        if let Some(tool) = &block.tool {
                            push_tool_card(&mut lines, tool, width);
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
    let shown = if streaming { format!("{text}▌") } else { text.to_owned() };
    for seg in wrap_text(&shown, width) {
        lines.push(RenderLine::plain(seg));
    }
}

fn push_reasoning_block(lines: &mut Vec<RenderLine>, text: &str, width: usize, streaming: bool) {
    if text.trim().is_empty() {
        return;
    }
    let wrapped = wrap_text(text, width.saturating_sub(2));
    let total = wrapped.len();
    let omitted = total.saturating_sub(REASONING_TAIL);
    if omitted > 0 {
        lines.push(
            RenderLine::new()
                .span("  ⋯ ", SpanStyle::Dim)
                .span(format!("思考（折叠 {omitted} 行，F3 展开）"), SpanStyle::Dim),
        );
    }
    let start = omitted.min(total);
    let tail = &wrapped[start..];
    let last = tail.len() - 1;
    for (i, seg) in tail.iter().enumerate() {
        let mut line = RenderLine::new().span("  ", SpanStyle::Dim);
        let shown = if streaming && i == last { format!("{seg}▌") } else { seg.clone() };
        line = line.span(shown, SpanStyle::Reasoning);
        lines.push(line);
    }
}

fn push_tool_card(lines: &mut Vec<RenderLine>, tool: &crate::app::timeline_model::ToolCard, width: usize) {
    let (icon, style) = match tool.state {
        TimelineToolState::Prepared => ("◌", SpanStyle::Dim),
        TimelineToolState::Running => ("◐", SpanStyle::ToolRun),
        TimelineToolState::Succeeded => ("●", SpanStyle::ToolOk),
        TimelineToolState::Failed => ("✗", SpanStyle::ToolFail),
    };
    lines.push(
        RenderLine::new()
            .span("  ", SpanStyle::Dim)
            .span(format!("{icon} "), style)
            .span(tool.name.clone(), SpanStyle::Accent)
            .span(format!(" · {:?}", tool.state), style),
    );

    if let Some(summary) = tool.summary.as_deref().filter(|s| !s.is_empty()) {
        for seg in wrap_text(summary, width.saturating_sub(6)) {
            lines.push(RenderLine::new().span("    ↳ ", SpanStyle::Dim).span(seg, SpanStyle::Plain));
        }
    }
    if let Some(args) = tool.args_json.as_deref().filter(|s| !s.is_empty() && *s != "{}") {
        let preview: String = if args.chars().count() > ARG_PREVIEW {
            format!("{}…", args.chars().take(ARG_PREVIEW).collect::<String>())
        } else {
            args.to_owned()
        };
        let one_line = preview.replace('\n', " ");
        for seg in wrap_text(&one_line, width.saturating_sub(6)) {
            lines.push(RenderLine::new().span("    ⌗ ", SpanStyle::Dim).span(seg, SpanStyle::Dim));
        }
    }

    if let Some(diff) = &tool.diff {
        let added = diff.lines().filter(|l| l.starts_with('+') && !l.starts_with("+++")).count();
        let removed = diff.lines().filter(|l| l.starts_with('-') && !l.starts_with("---")).count();
        lines.push(
            RenderLine::new()
                .span("    Δ ", SpanStyle::Dim)
                .span(format!("+{added}"), SpanStyle::DiffAdd)
                .span(" ", SpanStyle::Dim)
                .span(format!("−{removed}"), SpanStyle::DiffDel),
        );
    }

    let mut output_tail: Vec<&str> = Vec::new();
    if let Some(output) = tool.output.as_deref().filter(|s| !s.is_empty()) {
        output_tail.extend(output.lines());
    }
    if !tool.progress.is_empty() {
        output_tail.extend(tool.progress.lines());
    }
    if !output_tail.is_empty() {
        let start = output_tail.len().saturating_sub(OUTPUT_TAIL);
        let skipped = start;
        if skipped > 0 {
            lines.push(RenderLine::new().span(format!("    （输出省略 {skipped} 行）"), SpanStyle::Dim));
        }
        for out in &output_tail[start..] {
            for seg in wrap_text(out, width.saturating_sub(6)) {
                lines.push(RenderLine::new().span("    │ ", SpanStyle::Dim).span(seg, SpanStyle::Dim));
            }
        }
    }

    if let Some(err) = &tool.failure {
        for seg in wrap_text(&format!("{}: {}", err.code, err.message), width.saturating_sub(6)) {
            lines.push(RenderLine::new().span("    ✗ ", SpanStyle::Error).span(seg, SpanStyle::Error));
        }
    }
    if let Some(perm) = &tool.permission {
        lines.push(
            RenderLine::new()
                .span("    ⚠ ", SpanStyle::Warn)
                .span(format!("等待权限：{}（risk {}）", perm.category, perm.risk), SpanStyle::Warn),
        );
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
