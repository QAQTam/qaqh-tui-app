//! `/export`：会话 transcript → Markdown（M4）。
//!
//! 数据源 = **timeline 模型**（非渲染缓存）：输出与终端宽度无关、可跨端复用；
//! 展示投影（display）优先、H16 旧字段回退，与渲染层同一口径。
//!
//! v1 范围：仅当前窗口内的回合（`timeline.turns`）；更早回合的折叠/归档
//! 状态如实标注在头部（B1「丢弃必须可见」）。reasoning body 不驻留
//! （D1 有损客户端纪律）——思考信息以回合头的聚合计数呈现。

use crate::app::session::SessionState;
use crate::app::timeline_model::{Block, ToolCard, Turn};
use qaqh_client::{TimelineBlockKind, TimelineToolBody, TimelineToolState, TimelineTurnState};

/// 渲染整个会话为 Markdown。
pub(crate) fn export_markdown(sess: &SessionState) -> String {
    let mut out = String::new();
    out.push_str("# qaqh 会话导出\n\n");
    out.push_str(&format!("- 会话：`{}`\n", sess.seed));
    out.push_str(&format!(
        "- 回合数：{}（当前窗口内）\n",
        sess.timeline.turns.len()
    ));
    out.push_str(&format!(
        "- 导出时间：{}\n",
        chrono::Local::now().format("%Y-%m-%d %H:%M:%S")
    ));
    if sess.timeline.has_more {
        out.push_str("- ⚠ 更早回合已折叠（不在本文件内）\n");
    } else if sess.timeline.truncated_before {
        out.push_str("- ⚠ 更早回合未包含（仅存于 daemon 归档）\n");
    }
    out.push('\n');
    for (i, turn) in sess.timeline.turns.iter().enumerate() {
        export_turn(&mut out, i + 1, turn);
    }
    out
}

/// 单个回合的 Markdown（`/history` 的详情视图与「导出此回合」共用同一份文本，
/// 所以详情里看到的就是导出出去的内容）。
pub(crate) fn export_turn_markdown(turn: &Turn, number: usize) -> String {
    let mut out = String::new();
    export_turn(&mut out, number, turn);
    out
}

/// 默认导出路径：`./qaqh-export-{seed 前 8 位}-{时间戳}.md`。
pub(crate) fn default_export_path(seed: &str) -> std::path::PathBuf {
    let short: String = seed.chars().take(8).collect();
    let ts = chrono::Local::now().format("%Y%m%d-%H%M%S");
    std::path::PathBuf::from(format!("qaqh-export-{short}-{ts}.md"))
}

fn export_turn(out: &mut String, n: usize, turn: &Turn) {
    let mut head = format!("## Turn {n} · {}", turn_state_label(turn.state));
    if turn.thinking.segments > 0 {
        head.push_str(&format!(
            " · 思考 {} 段/{} 行",
            turn.thinking.segments, turn.thinking.lines
        ));
    }
    let (total, failed) = tool_counts(turn);
    if total > 0 {
        head.push_str(&format!(" · {total} 工具"));
        if failed > 0 {
            head.push_str(&format!("（{failed}✗）"));
        }
    }
    out.push_str(&head);
    out.push_str("\n\n");

    if turn.offloaded {
        out.push_str("> ⚠ 本回合已 offload——正文仅剩预览壳，完整内容存于 daemon。\n\n");
    }
    if let Some(f) = &turn.failure {
        out.push_str(&format!("> ✗ 失败（{}）：{}\n\n", f.code, f.message));
    }

    // 用户消息引用化（保留原文换行）。
    out.push_str("**❯ 用户**\n\n");
    if turn.user_text.trim().is_empty() {
        out.push_str("> （空）\n\n");
    } else {
        for line in turn.user_text.lines() {
            out.push_str("> ");
            out.push_str(line);
            out.push('\n');
        }
        out.push('\n');
    }

    for round in &turn.rounds {
        for block in &round.blocks {
            export_block(out, block);
        }
    }
    out.push_str("---\n\n");
}

fn export_block(out: &mut String, block: &Block) {
    match block.kind {
        TimelineBlockKind::Text => {
            if !block.text.trim().is_empty() {
                out.push_str(&block.text);
                out.push_str("\n\n");
            }
        }
        // D1：reasoning body 不驻留（聚合计数在回合头）；活动回合的 body 仅
        // 存在于 Ctrl+T 浮层，不进导出。
        TimelineBlockKind::Reasoning => {}
        TimelineBlockKind::Notice => {
            for line in block.text.lines() {
                out.push_str("> · ");
                out.push_str(line);
                out.push('\n');
            }
            out.push('\n');
        }
        TimelineBlockKind::Tool => {
            if let Some(tool) = &block.tool {
                export_tool(out, tool);
            }
        }
    }
}

fn export_tool(out: &mut String, tool: &ToolCard) {
    let d = tool.display.as_ref();
    let mut head = format!("### ⚙ {} · {}", tool.name, tool_state_label(tool.state));
    if let Some(m) = d.and_then(|d| d.metrics.as_ref()) {
        if m.elapsed_ms > 0 {
            head.push_str(&format!(" · {:.1}s", m.elapsed_ms as f64 / 1000.0));
        }
        if m.output_bytes > 0 {
            head.push_str(&format!(" · {}", human_bytes(m.output_bytes)));
        }
    }
    out.push_str(&head);
    out.push_str("\n\n");

    // header（投影优先）。
    if let Some(h) = d.and_then(|d| d.header.as_ref()) {
        use qaqh_client::TimelineToolHeader as H;
        let line = match h {
            H::Path { path, .. } => Some(format!("- 路径：`{path}`")),
            H::Shell { command } => Some(format!("- 命令：`{command}`")),
            H::Query { query, scope } => Some(match scope {
                Some(s) => format!("- 查询：`{query}`（{s}）"),
                None => format!("- 查询：`{query}`"),
            }),
            H::Other { label } => Some(format!("- {label}")),
            H::Unknown => None,
        };
        if let Some(line) = line {
            out.push_str(&line);
            out.push('\n');
        }
    }
    // summary（投影优先 → H16 旧字段回退）。
    let summary = d
        .and_then(|d| d.summary.as_deref())
        .filter(|s| !s.is_empty())
        .or(tool.summary.as_deref().filter(|s| !s.is_empty()));
    if let Some(s) = summary {
        out.push_str(&format!("- 摘要：{}", s.replace('\n', " ")));
        out.push('\n');
    }
    if let Some(args) = tool
        .args_json
        .as_deref()
        .filter(|s| !s.is_empty() && *s != "{}")
    {
        out.push_str(&format!("- 参数：`{args}`"));
        out.push('\n');
    }
    out.push('\n');

    // body（投影变体优先 → 旧字段回退）。
    let body = d.and_then(|d| d.body.as_ref());
    match body {
        Some(TimelineToolBody::Shell { output, .. }) => push_fenced(out, "sh", output),
        Some(TimelineToolBody::Streams { stdout, stderr, .. }) => {
            if !stdout.trim().is_empty() {
                out.push_str("stdout:\n");
                push_fenced(out, "", stdout);
            }
            if !stderr.trim().is_empty() {
                out.push_str("stderr:\n");
                push_fenced(out, "", stderr);
            }
        }
        Some(TimelineToolBody::Diff { unified, .. }) => push_fenced(out, "diff", unified),
        Some(TimelineToolBody::Text { text, .. }) => push_fenced(out, "", text),
        Some(TimelineToolBody::Subagent { name, .. }) => {
            out.push_str(&format!("（subagent：{name}）\n\n"));
        }
        Some(TimelineToolBody::None) => {}
        Some(TimelineToolBody::Unknown) => {}
        None => {
            if let Some(diff) = tool.diff.as_deref().filter(|s| !s.trim().is_empty()) {
                push_fenced(out, "diff", diff);
            } else if let Some(output) = tool.output.as_deref().filter(|s| !s.trim().is_empty()) {
                push_fenced(out, "", output);
            }
        }
    }

    if let Some(f) = &tool.failure {
        out.push_str(&format!("> ✗ 失败（{}）：{}\n\n", f.code, f.message));
    }
}

fn push_fenced(out: &mut String, lang: &str, text: &str) {
    let t = text.trim_end();
    if t.is_empty() {
        return;
    }
    out.push_str("```");
    out.push_str(lang);
    out.push('\n');
    out.push_str(t);
    out.push_str("\n```\n\n");
}

fn tool_counts(turn: &Turn) -> (usize, usize) {
    let mut total = 0;
    let mut failed = 0;
    for round in &turn.rounds {
        for block in &round.blocks {
            if block.kind == TimelineBlockKind::Tool && block.tool.is_some() {
                total += 1;
                if block
                    .tool
                    .as_ref()
                    .is_some_and(|t| t.state == TimelineToolState::Failed)
                {
                    failed += 1;
                }
            }
        }
    }
    (total, failed)
}

fn turn_state_label(s: TimelineTurnState) -> &'static str {
    match s {
        TimelineTurnState::Running => "进行中",
        TimelineTurnState::Completed => "已完成",
        TimelineTurnState::Failed => "失败",
        TimelineTurnState::Cancelled => "已取消",
    }
}

fn tool_state_label(s: TimelineToolState) -> &'static str {
    match s {
        TimelineToolState::Prepared => "已准备",
        TimelineToolState::Running => "运行中",
        TimelineToolState::Succeeded => "已完成",
        TimelineToolState::Failed => "失败",
        TimelineToolState::Cancelled => "已取消",
        TimelineToolState::Backgrounded => "后台运行",
    }
}

// human_bytes：与渲染层共用（契约 P2 展示口径）
use crate::app::render_transcript::human_bytes;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::timeline_model::{Block, Round, ToolCard, Turn};
    use qaqh_client::{
        TimelineBlockState, TimelineFailure, TimelineToolDisplay, TimelineToolState,
        TimelineTurnState,
    };

    fn block_text(id: &str, order: u32, text: &str) -> Block {
        Block {
            block_id: id.into(),
            block_order: order,
            kind: TimelineBlockKind::Text,
            state: TimelineBlockState::Sealed,
            text: text.into(),
            tool: None,
            last_fragment: 0,
            rev: 1,
        }
    }

    fn block_tool(id: &str, order: u32, tool: ToolCard) -> Block {
        Block {
            block_id: id.into(),
            block_order: order,
            kind: TimelineBlockKind::Tool,
            state: TimelineBlockState::Sealed,
            text: String::new(),
            tool: Some(tool),
            last_fragment: 0,
            rev: 1,
        }
    }

    fn card(name: &str, state: TimelineToolState) -> ToolCard {
        ToolCard {
            tool_call_id: format!("{name}-1"),
            name: name.into(),
            state,
            summary: None,
            args_json: None,
            output: None,
            diff: None,
            progress: String::new(),
            progress_truncated: false,
            progress_bytes_total: 0,
            progress_stream: None,
            failure: None,
            permission: None,
            display: None,
        }
    }

    fn sess_with(turns: Vec<Turn>) -> SessionState {
        let mut s = SessionState::new("seed-abcdef123".to_string());
        s.timeline.turns = turns;
        s
    }

    #[test]
    fn export_contains_turn_head_user_and_tool_card() {
        let mut t = Turn {
            turn_id: "t1".into(),
            turn_index: Some(1),
            user_text: "修复登录 bug".into(),
            state: TimelineTurnState::Completed,
            failure: None,
            sealed: true,
            offloaded: false,
            thinking: crate::app::timeline_model::ThinkingStats {
                segments: 3,
                lines: 120,
            },
            rounds: vec![Round {
                round_num: 0,
                sealed: true,
                is_final: true,
                blocks: vec![
                    block_text("b1", 0, "已修复。"),
                    {
                        let mut c = card("bash", TimelineToolState::Succeeded);
                        c.display = Some(TimelineToolDisplay {
                            summary: Some("cargo test 通过".into()),
                            diff: None,
                            header: Some(qaqh_client::TimelineToolHeader::Shell {
                                command: "cargo test".into(),
                            }),
                            body: Some(TimelineToolBody::Shell {
                                output: "202 passed".into(),
                                exit_code: Some(0),
                                truncated: false,
                            }),
                            metrics: None,
                            outcome: None,
                        });
                        block_tool("b2", 1, c)
                    },
                    {
                        let mut c = card("edit", TimelineToolState::Failed);
                        c.failure = Some(TimelineFailure {
                            code: "E1".into(),
                            message: "补丁不匹配".into(),
                        });
                        block_tool("b3", 2, c)
                    },
                ],
            }],
        };
        t.sealed = true;
        let sess = sess_with(vec![t]);
        let md = export_markdown(&sess);

        assert!(md.contains("# qaqh 会话导出"), "{md}");
        assert!(md.contains("- 会话：`seed-abcdef123`"), "{md}");
        assert!(
            md.contains("## Turn 1 · 已完成 · 思考 3 段/120 行 · 2 工具（1✗）"),
            "回合头聚合：{md}"
        );
        assert!(md.contains("**❯ 用户**"), "{md}");
        assert!(md.contains("> 修复登录 bug"), "用户引用：{md}");
        assert!(md.contains("已修复。"), "{md}");
        assert!(md.contains("### ⚙ bash · 已完成"), "{md}");
        assert!(md.contains("- 命令：`cargo test`"), "{md}");
        assert!(md.contains("- 摘要：cargo test 通过"), "{md}");
        assert!(md.contains("```sh\n202 passed\n```"), "shell body：{md}");
        assert!(md.contains("> ✗ 失败（E1）：补丁不匹配"), "失败内联：{md}");
    }

    #[test]
    fn export_marks_folded_history_and_offload() {
        let mut sess = sess_with(vec![]);
        sess.timeline.has_more = true;
        let md = export_markdown(&sess);
        assert!(md.contains("更早回合已折叠"), "{md}");

        let mut sess2 = sess_with(vec![]);
        sess2.timeline.truncated_before = true;
        let md2 = export_markdown(&sess2);
        assert!(md2.contains("仅存于 daemon 归档"), "{md2}");
    }

    #[test]
    fn default_path_shape() {
        let p = default_export_path("seed-abcdef123");
        let s = p.to_string_lossy().to_string();
        assert!(s.starts_with("qaqh-export-seed-abc-"), "{s}");
        assert!(s.ends_with(".md"), "{s}");
    }
}
