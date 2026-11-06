//! Adapts the canonical v3 `SessionState` / `SessionEvent` to the TUI's
//! transcript view.
//!
//! This is presentation state only. backend-v3 的读模型是单一 `SessionState`
//! （`turns` / `timeline` / `tools`），事件是单一 `SessionEventFrame` 流；
//! 两者都直接来自 `qaqh-ringing-contract`（权威契约直连，无镜像）。
//!
//! 时序事实（backend `session_loop.rs`）：`TurnStarted` 事件先于
//! `TimelineAppended(Input)` 到达。所以实时路径上回合的 user_text 以
//! Input 块为准：先缓冲回合身份（`pending_turn_text`），Input 到达时用
//! 真文本开回合；若流式块先到，则以空文本兜底开回合（Input 晚到时回填）。

use std::collections::{HashMap, HashSet};

use qaqh_ringing_contract::fact::{
    ActorKind, AssistantBlockKind, ContentValue, ToolCallOutcome, ToolError, ToolMetrics,
    ToolTerminalStatus, TurnTerminal,
};
use qaqh_ringing_contract::state::{
    TimelineEntry as WireEntry, ToolCallState, ToolCallView, TurnOutcome, TurnView,
};
use qaqh_ringing_contract::{SessionBootstrap, SessionEvent, SessionEventFrame};

use crate::app::session::SessionState;
use crate::app::timeline_compat::{
    TimelineBlock, TimelineBlockKind, TimelineBlockState, TimelineEntry, TimelineEvent,
    TimelineFailure, TimelinePage, TimelineRound, TimelineSnapshot, TimelineTool, TimelineToolBody,
    TimelineToolDisplay, TimelineToolMetrics, TimelineToolState, TimelineTurn, TimelineTurnState,
};

// ───────────────────────── bootstrap 基线 ─────────────────────────

/// Build the local transcript baseline from the v3 `SessionState` snapshot.
///
/// 压缩可能移除更早的 Input 块；那些回合保留 `[用户输入已折叠]` 标记。
/// timeline 的顺序就是权威顺序（无需再按 fact_seq 排序）。
pub fn page_from_bootstrap(
    bootstrap: &SessionBootstrap,
    content_cache: &HashMap<String, String>,
) -> TimelinePage {
    let state = &bootstrap.state;
    let tools_by_call: HashMap<&str, &ToolCallView> = state
        .tools
        .iter()
        .map(|tool| (tool.call_id.as_str(), tool))
        .collect();
    let has_compaction = state
        .timeline
        .iter()
        .any(|entry| matches!(entry, WireEntry::Compaction { .. }));

    // Input 块按回合归位（compaction 后缺失 → 折叠标记）。
    let mut inputs_by_turn: HashMap<String, String> = HashMap::new();
    // 每个 turn 的块列表：按 timeline 顺序构建。
    let mut blocks_by_turn: HashMap<String, Vec<TimelineBlock>> = HashMap::new();
    // 无 turn 归属的 timeline 条目（罕见）：归属到「最近的回合」，
    // 由下方按顺序遍历时填。
    let ordered_turn_ids: Vec<String> = state
        .turns
        .iter()
        .map(|turn| turn.turn_id.as_ref().to_owned())
        .collect();
    let mut orphan_blocks: Vec<TimelineBlock> = Vec::new();

    for entry in &state.timeline {
        match entry {
            WireEntry::Input {
                turn_id, content, ..
            } => {
                if let Some(turn_id) = turn_id {
                    inputs_by_turn.insert(
                        turn_id.as_ref().to_owned(),
                        content_text(content, content_cache),
                    );
                }
            }
            WireEntry::AssistantBlock {
                turn_id,
                block_id,
                block_kind,
                content,
                ..
            } => {
                // `AssistantBlock.turn_id` 是非 Option 的 `TurnId`（契约如此）。
                let turn_id = turn_id.as_ref();
                let kind = match block_kind {
                    AssistantBlockKind::Reasoning => TimelineBlockKind::Reasoning,
                    AssistantBlockKind::Answer => TimelineBlockKind::Text,
                    // 工具调用卡由 `tools` 列表 + `ToolCall` 条目承担。
                    AssistantBlockKind::ToolCall => continue,
                };
                blocks_by_turn
                    .entry(turn_id.to_owned())
                    .or_default()
                    .push(TimelineBlock {
                        block_id: block_id.as_ref().to_owned(),
                        block_order: 0,
                        kind,
                        state: TimelineBlockState::Sealed,
                        text: content_text(content, content_cache),
                        tool: None,
                        reasoning_kind: None,
                    });
            }
            WireEntry::ToolCall { call_id, turn_id } => {
                let tool_view = tools_by_call.get(call_id.as_str()).copied();
                let block = tool_block_from_view(
                    call_id.as_str(),
                    turn_opt_str(turn_id),
                    tool_view,
                    content_cache,
                );
                match turn_opt_str(turn_id) {
                    Some(turn_id) => blocks_by_turn
                        .entry(turn_id.to_owned())
                        .or_default()
                        .push(block),
                    None => orphan_blocks.push(block),
                }
            }
            WireEntry::ToolResult {
                call_id, content, ..
            } => {
                // 工具结果并入对应工具卡（卡由 ToolCall 条目建立）。
                let output = content_text(content, content_cache);
                for blocks in blocks_by_turn.values_mut() {
                    if let Some(block) = blocks
                        .iter_mut()
                        .find(|block| block.block_id == call_id.as_str())
                    {
                        if let Some(tool) = block.tool.as_mut() {
                            tool.output = Some(output.clone());
                        }
                    }
                }
            }
            WireEntry::Compaction { summary } => {
                let text = content_text(summary, content_cache);
                if text.is_empty() {
                    continue;
                }
                let block = TimelineBlock {
                    block_id: format!("compaction:{}", blocks_by_turn.len()),
                    block_order: 0,
                    kind: TimelineBlockKind::Notice,
                    state: TimelineBlockState::Sealed,
                    text: format!("上下文摘要\n{text}"),
                    tool: None,
                    reasoning_kind: None,
                };
                // 锚定「timeline 上最后一个有块的回合」，与 v2 同口径。
                let anchor = ordered_turn_ids
                    .iter()
                    .rev()
                    .find(|turn_id| blocks_by_turn.contains_key(*turn_id))
                    .cloned();
                match anchor {
                    Some(turn_id) => {
                        let blocks = blocks_by_turn.entry(turn_id).or_default();
                        let mut block = block;
                        block.block_order = blocks.len() as u32;
                        blocks.push(block);
                    }
                    None => orphan_blocks.push(block),
                }
            }
        }
    }

    // 补齐 block_order（timeline 顺序即块序）。
    for blocks in blocks_by_turn.values_mut() {
        for (order, block) in blocks.iter_mut().enumerate() {
            block.block_order = order as u32;
        }
    }

    let mut turns = Vec::with_capacity(state.turns.len());
    for (index, view) in state.turns.iter().enumerate() {
        let turn_id = view.turn_id.as_ref();
        let mut user_text = inputs_by_turn.get(turn_id).cloned().unwrap_or_default();
        let blocks = blocks_by_turn.remove(turn_id).unwrap_or_default();
        if has_compaction && user_text.is_empty() && !blocks.is_empty() {
            user_text = "[用户输入已折叠]".to_owned();
        }
        if has_compaction && user_text.is_empty() && blocks.is_empty() {
            // 压缩保留回合 outcome 但丢弃了上下文：跳过空回合，避免长串空行。
            continue;
        }
        let (turn_state, failure) = turn_outcome(view);
        let sealed = turn_state != TimelineTurnState::Running;
        turns.push(TimelineTurn {
            turn_id: turn_id.to_owned(),
            turn_index: Some(index as u64),
            created_seq: index as u64,
            user_text,
            sealed,
            offloaded: false,
            state: turn_state,
            failure,
            rounds: vec![TimelineRound {
                round_num: 0,
                sealed,
                is_final: sealed,
                blocks,
            }],
        });
    }

    // 孤儿块（无归属工具卡/压缩摘要）：挂到独立哨兵回合，保证不丢。
    if !orphan_blocks.is_empty() {
        turns.push(TimelineTurn {
            turn_id: "tui-orphan-blocks".to_owned(),
            turn_index: None,
            created_seq: state.seq,
            user_text: String::new(),
            sealed: true,
            offloaded: false,
            state: TimelineTurnState::Completed,
            failure: None,
            rounds: vec![TimelineRound {
                round_num: 0,
                sealed: true,
                is_final: true,
                blocks: orphan_blocks,
            }],
        });
    }

    TimelinePage {
        schema: qaqh_ringing::RINGING_SCHEMA.to_owned(),
        version: qaqh_ringing::RINGING_V2_VERSION,
        server_epoch: bootstrap.server_epoch.clone(),
        session_id: bootstrap.session_id.clone(),
        snapshot: TimelineSnapshot {
            watermark: state.seq,
            turns,
        },
        has_more: false,
        total_turns: state.turns.len(),
        truncated_before: has_compaction,
    }
}

// ───────────────────────── 实时事件 ─────────────────────────

/// Apply transcript-bearing session events. Other UI concerns remain in
/// `App::handle_session_event`.
pub fn apply_frame(session: &mut SessionState, frame: &SessionEventFrame) {
    match &frame.event {
        SessionEvent::TimelineAppended { entry } => {
            apply_timeline_entry(session, entry);
        }
        SessionEvent::MessageDelta {
            turn_id,
            block_id,
            block_kind,
            text,
        } => {
            let kind = match block_kind {
                AssistantBlockKind::Reasoning => TimelineBlockKind::Reasoning,
                AssistantBlockKind::Answer => TimelineBlockKind::Text,
                AssistantBlockKind::ToolCall => return,
            };
            ensure_turn(session, turn_id.as_ref());
            ensure_block(
                session,
                turn_id.as_ref(),
                TimelineBlock {
                    block_id: block_id.as_ref().to_owned(),
                    block_order: block_order(session, turn_id.as_ref(), block_id.as_ref()),
                    kind,
                    state: TimelineBlockState::Open,
                    text: String::new(),
                    tool: None,
                    reasoning_kind: None,
                },
            );
            // `MessageDelta` 是**替换**语义（同一 block_id 的后续事件替换已展示内容），
            // `BlockCheckpoint` 恰好是整帧替换。
            apply(
                session,
                turn_id.as_ref(),
                TimelineEvent::BlockCheckpoint {
                    block_id: block_id.as_ref().to_owned(),
                    arg: None,
                    text: text.clone(),
                },
            );
        }
        SessionEvent::ToolDeclared {
            call_id,
            turn_id,
            tool_name,
            args,
        } => {
            ensure_turn(session, turn_id.as_ref());
            let tool = new_tool(
                call_id.as_str(),
                tool_name,
                args.as_ref()
                    .map(|value| value.to_string())
                    .unwrap_or_default(),
                TimelineToolState::Running,
            );
            ensure_tool_block(session, turn_id.as_ref(), tool, TimelineBlockState::Open);
        }
        SessionEvent::ToolArgumentsDelta {
            call_id,
            turn_id,
            tool_name,
            text,
        } => {
            ensure_turn(session, turn_id.as_ref());
            // 累计参数前缀（替换语义）；声明事件尚未到达时先建卡。
            let tool = new_tool(
                call_id.as_str(),
                tool_name,
                text.clone(),
                TimelineToolState::Running,
            );
            update_tool(session, turn_id.as_ref(), tool);
        }
        SessionEvent::ToolStateChanged {
            call_id,
            turn_id,
            state,
        } => {
            ensure_turn(session, turn_id.as_ref());
            let tool_state = match state {
                ToolCallState::Finished { outcome } => tool_state_from_outcome(outcome),
                _ => TimelineToolState::Running,
            };
            update_tool_state(session, turn_id.as_ref(), call_id.as_str(), tool_state);
        }
        SessionEvent::ToolOutputDelta {
            call_id,
            text,
            ..
        } => {
            // 普通工具（无执行会话）的流式输出尾窗：追加进工具卡 progress
            //（有界：超出 8 KiB 后丢头部，只保尾部——这是预览，不是权威正文）。
            const PROGRESS_CAP: usize = 8 * 1024;
            if let Some(turn_id) = session.timeline.turns.iter().find_map(|turn| {
                turn.rounds
                    .iter()
                    .flat_map(|round| &round.blocks)
                    .any(|block| block.block_id == call_id.as_str())
                    .then(|| turn.turn_id.clone())
            }) {
                if let Some(turn) = session
                    .timeline
                    .turns
                    .iter_mut()
                    .find(|turn| turn.turn_id == turn_id)
                {
                    for block in turn.rounds.iter_mut().flat_map(|round| &mut round.blocks) {
                        if block.block_id == call_id.as_str()
                            && let Some(card) = block.tool.as_mut()
                        {
                            card.progress.push_str(text);
                            if card.progress.len() > PROGRESS_CAP {
                                let cut = card.progress.len() - PROGRESS_CAP;
                                let boundary = (cut..=card.progress.len())
                                    .find(|&i| card.progress.is_char_boundary(i))
                                    .unwrap_or(cut);
                                card.progress = card.progress[boundary..].to_owned();
                                card.progress_truncated = true;
                            }
                        }
                    }
                    session.timeline.bump();
                }
            }
        }
        SessionEvent::ToolFinished {
            call_id,
            outcome,
            output,
            metrics,
            code_delta,
            ..
        } => {
            finish_tool_card(
                session,
                call_id.as_str(),
                outcome,
                output.as_ref(),
                metrics.as_ref(),
                code_delta.as_ref(),
            );
        }
        SessionEvent::TurnEnded { turn_id, outcome } => {
            let (state, failure) = match outcome {
                TurnOutcome::Finished {
                    terminal, error, ..
                } => (
                    turn_terminal_state(*terminal),
                    error.as_ref().map(|error| TimelineFailure {
                        code: error.code.clone(),
                        message: error.message.clone(),
                    }),
                ),
                TurnOutcome::Interrupted { .. } => (TimelineTurnState::Cancelled, None),
            };
            apply(
                session,
                turn_id.as_ref(),
                TimelineEvent::TurnSealed { state, failure },
            );
            session.current_rounds.remove(turn_id.as_ref());
        }
        _ => {}
    }
}

/// `TimelineAppended`：回放/补投的 timeline 条目（与实时事件同源同形状）。
fn apply_timeline_entry(session: &mut SessionState, entry: &WireEntry) {
    match entry {
        WireEntry::Input {
            turn_id, content, ..
        } => {
            let text = content_text(content, &session.transcript_content);
            match turn_opt_str(turn_id) {
                Some(turn_id) => open_or_fill_turn(session, turn_id.as_ref(), text),
                // QueueOnly 的排队输入：等派发时以带 turn_id 的新条目落盘，
                // 这里只收内容引用，不上屏。
                None => {}
            }
        }
        WireEntry::AssistantBlock {
            turn_id,
            block_id,
            block_kind,
            content,
            ..
        } => {
            let turn_id = turn_id.as_ref();
            let kind = match block_kind {
                AssistantBlockKind::Reasoning => TimelineBlockKind::Reasoning,
                AssistantBlockKind::Answer => TimelineBlockKind::Text,
                AssistantBlockKind::ToolCall => return,
            };
            ensure_turn(session, turn_id.as_ref());
            apply(
                session,
                turn_id.as_ref(),
                TimelineEvent::BlockOpened {
                    block: TimelineBlock {
                        block_order: block_order(session, turn_id.as_ref(), block_id.as_ref()),
                        block_id: block_id.as_ref().to_owned(),
                        kind,
                        state: TimelineBlockState::Sealed,
                        text: content_text(content, &session.transcript_content),
                        tool: None,
                        reasoning_kind: None,
                    },
                },
            );
        }
        WireEntry::ToolCall { call_id, turn_id } => {
            let Some(turn_id) = turn_opt_str(turn_id) else {
                return;
            };
            ensure_turn(session, turn_id.as_ref());
            let tool = new_tool(
                call_id.as_str(),
                "tool",
                String::new(),
                TimelineToolState::Running,
            );
            ensure_tool_block(session, turn_id.as_ref(), tool, TimelineBlockState::Open);
        }
        WireEntry::ToolResult {
            call_id, content, ..
        } => {
            // 结果补投：找到卡所在回合后回填输出并封口。
            let turn_id = session.timeline.turns.iter().find_map(|turn| {
                turn.rounds
                    .iter()
                    .flat_map(|round| &round.blocks)
                    .any(|block| block.block_id == call_id.as_str())
                    .then(|| turn.turn_id.clone())
            });
            let Some(turn_id) = turn_id else {
                return;
            };
            let output = content_text(content, &session.transcript_content);
            let tool = sealed_tool_with_output(call_id.as_str(), output);
            update_tool(session, &turn_id, tool);
            apply(
                session,
                &turn_id,
                TimelineEvent::BlockSealed {
                    block_id: call_id.as_str().to_owned(),
                },
            );
        }
        WireEntry::Compaction { summary } => {
            let text = content_text(summary, &session.transcript_content);
            if text.is_empty() {
                return;
            }
            let turn_id = session
                .timeline
                .turns
                .last()
                .map(|turn| turn.turn_id.clone())
                .unwrap_or_else(|| "tui-compaction".to_owned());
            let turn_exists = session
                .timeline
                .turns
                .iter()
                .any(|turn| turn.turn_id == turn_id);
            if !turn_exists {
                apply(
                    session,
                    &turn_id,
                    TimelineEvent::TurnOpened {
                        user_text: "历史上下文摘要".to_owned(),
                    },
                );
            }
            ensure_block(
                session,
                &turn_id,
                TimelineBlock {
                    block_id: format!("compaction:{}", session.timeline.version),
                    block_order: u32::MAX,
                    kind: TimelineBlockKind::Notice,
                    state: TimelineBlockState::Sealed,
                    text: format!("上下文摘要\n{text}"),
                    tool: None,
                    reasoning_kind: None,
                },
            );
        }
    }
}

// ───────────────────────── 内容引用收集 ─────────────────────────

pub fn content_ids_from_bootstrap(bootstrap: &SessionBootstrap) -> HashSet<String> {
    let mut ids = HashSet::new();
    for entry in &bootstrap.state.timeline {
        collect_entry_content_ids(entry, &mut ids);
    }
    for tool in &bootstrap.state.tools {
        if let Some(output) = &tool.output {
            collect_content_id(output, &mut ids);
        }
    }
    ids
}

pub fn content_ids_from_frame(frame: &SessionEventFrame) -> HashSet<String> {
    let mut ids = HashSet::new();
    match &frame.event {
        SessionEvent::TimelineAppended { entry } => collect_entry_content_ids(entry, &mut ids),
        SessionEvent::ToolFinished {
            output: Some(output),
            ..
        } => collect_content_id(output, &mut ids),
        _ => {}
    }
    ids
}

fn collect_entry_content_ids(entry: &WireEntry, ids: &mut HashSet<String>) {
    match entry {
        WireEntry::Input { content, .. } | WireEntry::ToolResult { content, .. } => {
            collect_content_id(content, ids)
        }
        WireEntry::AssistantBlock { content, .. } => collect_content_id(content, ids),
        WireEntry::Compaction { summary } => collect_content_id(summary, ids),
        WireEntry::ToolCall { .. } => {}
    }
}

fn collect_content_id(value: &ContentValue, ids: &mut HashSet<String>) {
    if let ContentValue::Ref { content_ref } = value {
        ids.insert(content_ref.hash().as_str().to_owned());
    }
}

pub fn apply_hydrated_content(session: &mut SessionState, content_id: &str, text: &str) {
    let marker = format!("[正文保存在内容存储：{content_id}]");
    let mut changed = false;
    for value in session.pending_input_text.values_mut() {
        changed |= replace_marker(value, &marker, text);
    }
    for turn in &mut session.timeline.turns {
        changed |= replace_marker(&mut turn.user_text, &marker, text);
        for block in turn.rounds.iter_mut().flat_map(|round| &mut round.blocks) {
            let mut block_changed = replace_marker(&mut block.text, &marker, text);
            let Some(tool) = block.tool.as_mut() else {
                if block_changed {
                    block.touch();
                }
                changed |= block_changed;
                continue;
            };
            if let Some(args) = &mut tool.args_json {
                block_changed |= replace_marker(args, &marker, text);
            }
            if let Some(output) = &mut tool.output {
                block_changed |= replace_marker(output, &marker, text);
            }
            if let Some(diff) = &mut tool.diff {
                block_changed |= replace_marker(diff, &marker, text);
            }
            if let Some(display) = tool.display.as_mut() {
                if let Some(summary) = display.summary.as_mut() {
                    block_changed |= replace_marker(summary, &marker, text);
                }
                if let Some(diff) = display.diff.as_mut() {
                    block_changed |= replace_marker(diff, &marker, text);
                }
                if let Some(body) = display.body.as_mut() {
                    match body {
                        TimelineToolBody::Text { text: body, .. } => {
                            block_changed |= replace_marker(body, &marker, text);
                        }
                        TimelineToolBody::Diff { unified, .. } => {
                            block_changed |= replace_marker(unified, &marker, text);
                        }
                        TimelineToolBody::Shell { output, .. } => {
                            block_changed |= replace_marker(output, &marker, text);
                        }
                        TimelineToolBody::Streams { stdout, stderr, .. } => {
                            block_changed |= replace_marker(stdout, &marker, text);
                            block_changed |= replace_marker(stderr, &marker, text);
                        }
                        TimelineToolBody::None
                        | TimelineToolBody::Subagent { .. }
                        | TimelineToolBody::Unknown => {}
                    }
                }
            }
            if block_changed {
                block.touch();
            }
            changed |= block_changed;
        }
    }
    if changed {
        session.timeline.bump();
    }
}

fn replace_marker(value: &mut String, marker: &str, text: &str) -> bool {
    if value.contains(marker) {
        *value = value.replace(marker, text);
        true
    } else {
        false
    }
}

// ───────────────────────── 映射助手 ─────────────────────────


/// `Option<TurnId>` → `Option<&str>`（`TurnId` 只实现 `AsRef<str>`，无 `Deref`）。
fn turn_opt_str(turn_id: &Option<qaqh_ringing_contract::fact::TurnId>) -> Option<&str> {
    turn_id.as_ref().map(|id| id.as_str())
}

fn content_text(value: &ContentValue, content_cache: &HashMap<String, String>) -> String {
    match value {
        ContentValue::Inline { text } => text.clone(),
        ContentValue::Ref { content_ref } => {
            let hash = content_ref.hash().as_str();
            content_cache
                .get(hash)
                .cloned()
                .unwrap_or_else(|| format!("[正文保存在内容存储：{hash}]"))
        }
        ContentValue::Unavailable(_) => "[正文已不可用]".to_owned(),
    }
}

fn turn_outcome(view: &TurnView) -> (TimelineTurnState, Option<TimelineFailure>) {
    match view.outcome.as_ref() {
        None => (TimelineTurnState::Running, None),
        Some(TurnOutcome::Finished {
            terminal, error, ..
        }) => {
            let state = turn_terminal_state(*terminal);
            let failure = error.as_ref().map(|error| TimelineFailure {
                code: error.code.clone(),
                message: error.message.clone(),
            });
            (state, failure)
        }
        Some(TurnOutcome::Interrupted { .. }) => (TimelineTurnState::Cancelled, None),
    }
}

fn turn_terminal_state(terminal: TurnTerminal) -> TimelineTurnState {
    match terminal {
        TurnTerminal::Completed => TimelineTurnState::Completed,
        TurnTerminal::Failed => TimelineTurnState::Failed,
        TurnTerminal::Cancelled => TimelineTurnState::Cancelled,
    }
}

fn tool_state_from_outcome(outcome: &ToolCallOutcome) -> TimelineToolState {
    match outcome {
        ToolCallOutcome::Completed => TimelineToolState::Succeeded,
        ToolCallOutcome::Yielded { .. } => TimelineToolState::Backgrounded,
        ToolCallOutcome::Failed { .. } => TimelineToolState::Failed,
        ToolCallOutcome::Cancelled { .. } | ToolCallOutcome::Interrupted { .. } => {
            TimelineToolState::Cancelled
        }
    }
}

fn tool_failure_from_outcome(outcome: &ToolCallOutcome) -> Option<TimelineFailure> {
    match outcome {
        ToolCallOutcome::Failed { error } => Some(TimelineFailure {
            code: error.code.clone(),
            message: error.message.clone(),
        }),
        _ => None,
    }
}

#[allow(dead_code)]
fn terminal_status_from_outcome(outcome: &ToolCallOutcome) -> Option<ToolTerminalStatus> {
    match outcome {
        ToolCallOutcome::Completed => Some(ToolTerminalStatus::Succeeded),
        ToolCallOutcome::Yielded { .. } => Some(ToolTerminalStatus::Backgrounded),
        ToolCallOutcome::Failed { .. } => Some(ToolTerminalStatus::Failed),
        ToolCallOutcome::Cancelled { .. } => Some(ToolTerminalStatus::Cancelled),
        ToolCallOutcome::Interrupted { .. } => Some(ToolTerminalStatus::Indeterminate),
    }
}

fn tool_metrics(metrics: &ToolMetrics) -> TimelineToolMetrics {
    TimelineToolMetrics {
        elapsed_ms: metrics
            .finished_at_ms
            .saturating_sub(metrics.started_at_ms)
            .max(0) as u64,
        output_bytes: metrics.output_bytes,
        retry_count: metrics.retry_count,
        effective_tool_name: None,
        user_initiated: false,
    }
}

fn new_tool(id: &str, name: &str, args_json: String, state: TimelineToolState) -> TimelineTool {
    TimelineTool {
        tool_call_id: id.to_owned(),
        name: name.to_owned(),
        state,
        summary: None,
        args_json: Some(args_json),
        output: None,
        diff: None,
        progress: String::new(),
        progress_truncated: false,
        progress_stream: None,
        progress_bytes_total: 0,
        display: None,
        failure: None,
        permission: None,
        exit_code: None,
        completed_at_ms: None,
    }
}

/// bootstrap 的 `ToolCallView` → 工具卡。声明即建卡；结果在 `ToolResult` 条目里。
fn tool_block_from_view(
    call_id: &str,
    _turn_id: Option<&str>,
    view: Option<&ToolCallView>,
    content_cache: &HashMap<String, String>,
) -> TimelineBlock {
    let (name, args_json, state, failure, _metrics) = match view {
        Some(view) => {
            let (state, failure) = match &view.state {
                ToolCallState::Finished { outcome } => {
                    (tool_state_from_outcome(outcome), tool_failure_from_outcome(outcome))
                }
                ToolCallState::Declared | ToolCallState::AwaitingInteraction { .. } => {
                    (TimelineToolState::Prepared, None)
                }
                ToolCallState::Running | ToolCallState::Cancelling => {
                    (TimelineToolState::Running, None)
                }
            };
            (
                view.tool_name.clone().unwrap_or_else(|| "tool".to_owned()),
                view.args.as_ref().map(|value| value.to_string()),
                state,
                failure,
                view.metrics.clone(),
            )
        }
        None => (
            "tool".to_owned(),
            None,
            TimelineToolState::Running,
            None,
            None,
        ),
    };
    let mut tool = new_tool(call_id, &name, args_json.unwrap_or_default(), state);
    tool.failure = failure;
    if let Some(view) = view {
        if let Some(output) = &view.output {
            tool.output = Some(content_text(output, content_cache));
        }
        if let Some(metrics) = &view.metrics {
            tool.display = Some(TimelineToolDisplay {
                summary: None,
                diff: None,
                lines_added: 0,
                lines_removed: 0,
                header: None,
                body: tool.output.clone().map(|text| TimelineToolBody::Text {
                    text,
                    truncated: view.output_truncated,
                }),
                metrics: Some(tool_metrics(metrics)),
                outcome: None,
            });
            tool.completed_at_ms = Some(metrics.finished_at_ms.max(0) as u64);
        }
        // edit 类工具的结构化 diff 统计（backend-v3 `CodeDelta`）。
        if let Some(delta) = &view.code_delta {
            tool.diff = Some(format!(
                "+{} -{}（新建 {}，删除 {} 文件）",
                delta.lines_added, delta.lines_removed, delta.files_created, delta.files_deleted
            ));
            if let Some(display) = tool.display.as_mut() {
                display.diff = tool.diff.clone();
                display.lines_added = delta.lines_added as u32;
                display.lines_removed = delta.lines_removed as u32;
            }
        }
    }
    let terminal = state != TimelineToolState::Running && state != TimelineToolState::Prepared;
    TimelineBlock {
        block_id: call_id.to_owned(),
        block_order: 0,
        kind: TimelineBlockKind::Tool,
        state: if terminal {
            TimelineBlockState::Sealed
        } else {
            TimelineBlockState::Open
        },
        text: String::new(),
        tool: Some(tool),
        reasoning_kind: None,
    }
}

fn sealed_tool_with_output(call_id: &str, output: String) -> TimelineTool {
    let mut tool = new_tool(call_id, "tool", String::new(), TimelineToolState::Succeeded);
    tool.output = Some(output);
    tool
}

fn finish_tool_card(
    session: &mut SessionState,
    call_id: &str,
    outcome: &ToolCallOutcome,
    output: Option<&ContentValue>,
    metrics: Option<&ToolMetrics>,
    code_delta: Option<&qaqh_ringing_contract::CodeDelta>,
) {
    // 权限面板按 call_id 随工具终态一并下架（与 v2 ToolFinished 行为一致）。
    session.resolve_permission(call_id);
    let Some(turn_id) = session.timeline.turns.iter().find_map(|turn| {
        turn.rounds
            .iter()
            .flat_map(|round| &round.blocks)
            .any(|block| block.block_id == call_id)
            .then(|| turn.turn_id.clone())
    }) else {
        return;
    };
    let (name, args_json) = session
        .timeline
        .turns
        .iter()
        .flat_map(|turn| &turn.rounds)
        .flat_map(|round| &round.blocks)
        .find(|block| block.block_id == call_id)
        .and_then(|block| block.tool.as_ref())
        .map(|tool| {
            (
                tool.name.clone(),
                tool.args_json.clone().unwrap_or_default(),
            )
        })
        .unwrap_or_else(|| ("tool".to_owned(), String::new()));
    let mut tool = new_tool(
        call_id,
        &name,
        args_json,
        tool_state_from_outcome(outcome),
    );
    tool.failure = tool_failure_from_outcome(outcome);
    tool.output = output.map(|value| content_text(value, &session.transcript_content));
    if let Some(metrics) = metrics {
        tool.display = Some(TimelineToolDisplay {
            summary: None,
            diff: None,
            lines_added: 0,
            lines_removed: 0,
            header: None,
            body: tool.output.clone().map(|text| TimelineToolBody::Text {
                text,
                truncated: false,
            }),
            metrics: Some(tool_metrics(metrics)),
            outcome: None,
        });
        tool.completed_at_ms = Some(metrics.finished_at_ms.max(0) as u64);
    }
    // edit 类工具的结构化 diff 统计（backend-v3 `CodeDelta`）。
    if let Some(delta) = code_delta {
        tool.diff = Some(format!(
            "+{} -{}（新建 {}，删除 {} 文件）",
            delta.lines_added, delta.lines_removed, delta.files_created, delta.files_deleted
        ));
        if let Some(display) = tool.display.as_mut() {
            display.diff = tool.diff.clone();
            display.lines_added = delta.lines_added as u32;
            display.lines_removed = delta.lines_removed as u32;
        }
    }
    update_tool(session, &turn_id, tool);
    apply(
        session,
        &turn_id,
        TimelineEvent::BlockSealed {
            block_id: call_id.to_owned(),
        },
    );
}

fn update_tool_state(
    session: &mut SessionState,
    turn_id: &str,
    call_id: &str,
    state: TimelineToolState,
) {
    // timeline_model::Block.tool 是内部 `ToolCard`（渲染模型）。状态迁移只改
    // state 字段——不改名/不重建卡，ToolCard 与 TimelineTool 的转换只在
    // `Block::from_wire` 单点发生。
    let Some(turn) = session
        .timeline
        .turns
        .iter_mut()
        .find(|turn| turn.turn_id == turn_id)
    else {
        return;
    };
    for block in turn.rounds.iter_mut().flat_map(|round| &mut round.blocks) {
        if block.block_id == call_id
            && let Some(card) = block.tool.as_mut()
        {
            // `ToolCard.state` 与 TimelineToolState 同一类型（渲染层直用权威词表）。
            card.state = state;
        }
    }
}

// ───────────────────────── 回合/块落地助手 ─────────────────────────

/// TurnStarted 先于 Input 块到达：开回合推迟到 Input 到达（真文本），
/// 流式块先到时空文本兜底开回合（`ensure_turn`）。
fn open_or_fill_turn(session: &mut SessionState, turn_id: &str, text: String) {
    let already_open = session
        .timeline
        .turns
        .iter()
        .any(|turn| turn.turn_id == turn_id);
    if !already_open {
        apply(
            session,
            turn_id,
            TimelineEvent::TurnOpened {
                user_text: text.clone(),
            },
        );
        session.pending_input_text.remove(turn_id);
        return;
    }
    // 已开且尚无文本（TurnStarted 先到、空文本兜底开启）：回填。
    if let Some(turn) = session
        .timeline
        .turns
        .iter_mut()
        .find(|turn| turn.turn_id == turn_id)
        && turn.user_text.is_empty()
    {
        turn.user_text = text;
        session.timeline.bump();
    }
}

/// 流式块先于 Input 块到达时的兜底开回合（空文本；Input 晚到回填）。
fn ensure_turn(session: &mut SessionState, turn_id: &str) {
    let already_open = session
        .timeline
        .turns
        .iter()
        .any(|turn| turn.turn_id == turn_id);
    if !already_open {
        apply(
            session,
            turn_id,
            TimelineEvent::TurnOpened {
                user_text: String::new(),
            },
        );
    }
}

fn block_order(session: &SessionState, turn_id: &str, block_id: &str) -> u32 {
    let round_num = session.current_rounds.get(turn_id).copied().unwrap_or(0);
    session
        .timeline
        .turns
        .iter()
        .find(|turn| turn.turn_id == turn_id)
        .and_then(|turn| {
            turn.rounds
                .iter()
                .find(|round| round.round_num == round_num)
        })
        .map(|round| {
            round
                .blocks
                .iter()
                .find(|block| block.block_id == block_id)
                .map(|block| block.block_order)
                .unwrap_or_else(|| {
                    round
                        .blocks
                        .iter()
                        .map(|block| block.block_order)
                        .max()
                        .map_or(0, |n| n + 1)
                })
        })
        .unwrap_or(0)
}

fn has_block(session: &SessionState, turn_id: &str, block_id: &str) -> bool {
    session
        .timeline
        .turns
        .iter()
        .find(|turn| turn.turn_id == turn_id)
        .is_some_and(|turn| {
            turn.rounds
                .iter()
                .flat_map(|round| &round.blocks)
                .any(|block| block.block_id == block_id)
        })
}

fn ensure_block(session: &mut SessionState, turn_id: &str, block: TimelineBlock) {
    if !has_block(session, turn_id, &block.block_id) {
        apply(session, turn_id, TimelineEvent::BlockOpened { block });
    }
}

fn ensure_tool_block(
    session: &mut SessionState,
    turn_id: &str,
    tool: TimelineTool,
    state: TimelineBlockState,
) {
    let id = tool.tool_call_id.clone();
    ensure_block(
        session,
        turn_id,
        TimelineBlock {
            block_order: block_order(session, turn_id, &id),
            block_id: id,
            kind: TimelineBlockKind::Tool,
            state,
            text: String::new(),
            tool: Some(tool),
            reasoning_kind: None,
        },
    );
}

fn update_tool(session: &mut SessionState, turn_id: &str, tool: TimelineTool) {
    let id = tool.tool_call_id.clone();
    if !has_block(session, turn_id, &id) {
        ensure_tool_block(session, turn_id, tool.clone(), TimelineBlockState::Open);
    }
    apply(
        session,
        turn_id,
        TimelineEvent::ToolUpdated { block_id: id, tool },
    );
}

fn apply(session: &mut SessionState, turn_id: &str, event: TimelineEvent) {
    let seq = session.timeline.version.saturating_add(1);
    let round_num = session.current_rounds.get(turn_id).copied().unwrap_or(0);
    let entry = TimelineEntry {
        timeline_seq: seq,
        turn_id: turn_id.to_owned(),
        round_num: Some(round_num),
        event,
    };
    if let Some(terminal) = session.timeline.apply(&entry) {
        crate::app::session::streaming_done(session, Some(&terminal.turn_id));
    }
    crate::app::session::sync_streaming_from_timeline(session);
}

// ───────────────────────── 死代码防波堤 ─────────────────────────
//
// `ActorKind` / `ToolError` / `ToolTerminalStatus` 来自同一权威模块，保留
// 显式引用以锁定词表可见性（后端改词表时这里是编译锚点）。
#[allow(dead_code)]
fn _word_list_anchor(kind: ActorKind, error: &ToolError, status: ToolTerminalStatus) -> String {
    let _ = (kind, status);
    error.code.clone()
}
