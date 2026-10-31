//! Adapts the canonical v2 conversation projection to the TUI's transcript view.
//!
//! This is presentation state only. The backend no longer has a timeline API;
//! bootstrap snapshots and conversation deltas are the source of transcript rows.

use std::collections::{HashMap, HashSet};

use crate::app::session::SessionState;
use crate::app::timeline_compat::{
    TimelineBlock, TimelineBlockKind, TimelineBlockState, TimelineEntry, TimelineEvent,
    TimelineFailure, TimelinePage, TimelineRound, TimelineSnapshot, TimelineTool, TimelineToolBody,
    TimelineToolDisplay, TimelineToolMetrics, TimelineToolState, TimelineTurn, TimelineTurnState,
};
use qaqh_client::{ClientV2Bootstrap, ContentValue, ConversationDelta};

/// Build the local transcript baseline from the current conversation projection.
///
/// The backend snapshot is a context projection, so compaction may have removed
/// older input/block details. Those turns remain visible with a compacted marker.
pub fn page_from_bootstrap(
    bootstrap: &ClientV2Bootstrap,
    content_cache: &HashMap<String, String>,
) -> TimelinePage {
    let snapshot = &bootstrap.conversation.state;
    let mut inputs = HashMap::<String, String>::new();
    for entry in &snapshot.context {
        if let qaqh_client::ConversationContextKind::Input(input) = &entry.kind {
            inputs.insert(
                input.input_id.as_str().to_owned(),
                content_text(&input.content, content_cache),
            );
        }
    }
    let mut context_by_turn: HashMap<String, Vec<_>> = HashMap::new();
    for entry in &snapshot.context {
        let turn_id = match &entry.kind {
            qaqh_client::ConversationContextKind::AssistantBlock(block) => {
                Some(block.turn_id.as_str())
            }
            qaqh_client::ConversationContextKind::ToolCall(tool) => {
                tool.turn_id.as_ref().map(|id| id.as_str())
            }
            _ => None,
        };
        if let Some(turn_id) = turn_id {
            context_by_turn
                .entry(turn_id.to_owned())
                .or_default()
                .push(entry);
        }
    }
    for entries in context_by_turn.values_mut() {
        entries.sort_by_key(|entry| entry.source_fact_seq);
    }

    let mut turns = Vec::with_capacity(snapshot.turns.len());
    for (index, state) in snapshot.turns.iter().enumerate() {
        let input_id = state.input_id.as_ref().map(|id| id.as_str());
        let mut user_text = input_id
            .and_then(|id| inputs.get(id).cloned())
            .unwrap_or_default();

        let mut blocks = Vec::new();
        for entry in context_by_turn
            .get(state.turn_id.as_str())
            .into_iter()
            .flatten()
        {
            match &entry.kind {
                qaqh_client::ConversationContextKind::AssistantBlock(block) => {
                    let kind = match block.block_kind {
                        qaqh_client::fact::AssistantBlockKind::Reasoning => {
                            TimelineBlockKind::Reasoning
                        }
                        qaqh_client::fact::AssistantBlockKind::Answer => TimelineBlockKind::Text,
                        qaqh_client::fact::AssistantBlockKind::ToolCall => continue,
                    };
                    blocks.push(TimelineBlock {
                        block_id: block.block_id.as_str().to_owned(),
                        block_order: blocks.len() as u32,
                        kind,
                        state: TimelineBlockState::Sealed,
                        text: content_text(&block.content, content_cache),
                        tool: None,
                        reasoning_kind: None,
                    });
                }
                qaqh_client::ConversationContextKind::ToolCall(tool) => {
                    blocks.push(tool_block(
                        tool.call_id.as_str(),
                        tool.tool_name.as_deref().unwrap_or("tool"),
                        tool.args
                            .as_ref()
                            .map(|value| content_text(value, content_cache))
                            .unwrap_or_default(),
                        tool.result
                            .as_ref()
                            .map(|result| timeline_tool_state(result.terminal_status))
                            .unwrap_or(TimelineToolState::Running),
                        tool.result.as_ref().and_then(|result| {
                            result
                                .output
                                .as_ref()
                                .map(|value| content_text(value, content_cache))
                        }),
                        tool.result.as_ref().and_then(|result| {
                            result.error.as_ref().map(|error| TimelineFailure {
                                code: error.code.clone(),
                                message: error.message.clone(),
                            })
                        }),
                        tool.result
                            .as_ref()
                            .map(|result| tool_metrics(&result.metrics)),
                        blocks.len() as u32,
                    ));
                }
                _ => {}
            }
        }

        if snapshot.compaction.is_some() && user_text.is_empty() && !blocks.is_empty() {
            user_text = "[用户输入已折叠]".to_owned();
        }
        if snapshot.compaction.is_some() && user_text.is_empty() && blocks.is_empty() {
            // The projection keeps turn outcomes after dropping compacted context.
            // Avoid rendering a long run of empty historical turns.
            continue;
        }

        let (turn_state, failure) = turn_outcome(state.outcome.as_ref());
        let sealed = turn_state != TimelineTurnState::Running;
        turns.push(TimelineTurn {
            turn_id: state.turn_id.as_str().to_owned(),
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

    if let Some(compaction) = snapshot.compaction.as_ref() {
        let summary = content_text(&compaction.summary, content_cache);
        if !summary.is_empty() {
            let block = TimelineBlock {
                block_id: format!("compaction:{}", compaction.checkpoint_id.as_str()),
                block_order: 0,
                kind: TimelineBlockKind::Notice,
                state: TimelineBlockState::Sealed,
                text: format!("上下文摘要\n{summary}"),
                tool: None,
                reasoning_kind: None,
            };
            if let Some(turn) = turns.last_mut() {
                let round = turn
                    .rounds
                    .first_mut()
                    .expect("each adapted turn has a round");
                let block = TimelineBlock {
                    block_order: round.blocks.len() as u32,
                    ..block
                };
                round.blocks.push(block);
            } else {
                turns.push(TimelineTurn {
                    turn_id: format!("tui-compaction-{}", compaction.checkpoint_id.as_str()),
                    turn_index: None,
                    created_seq: snapshot.last_fact_seq,
                    user_text: "历史上下文摘要".to_owned(),
                    sealed: true,
                    offloaded: false,
                    state: TimelineTurnState::Completed,
                    failure: None,
                    rounds: vec![TimelineRound {
                        round_num: 0,
                        sealed: true,
                        is_final: true,
                        blocks: vec![block],
                    }],
                });
            }
        }
    }

    TimelinePage {
        schema: qaqh_client::RINGING_SCHEMA.to_owned(),
        version: qaqh_client::RINGING_V2_VERSION,
        server_epoch: bootstrap.server_epoch.clone(),
        session_id: bootstrap.session_id.clone(),
        snapshot: TimelineSnapshot {
            watermark: snapshot.last_fact_seq,
            turns,
        },
        has_more: false,
        total_turns: snapshot.turns.len(),
        truncated_before: snapshot.compaction.is_some(),
    }
}

pub fn pending_inputs_from_bootstrap(
    bootstrap: &ClientV2Bootstrap,
    content_cache: &HashMap<String, String>,
) -> HashMap<String, String> {
    let consumed: std::collections::HashSet<_> = bootstrap
        .conversation
        .state
        .turns
        .iter()
        .filter_map(|turn| turn.input_id.as_ref().map(|id| id.as_str().to_owned()))
        .collect();
    bootstrap
        .conversation
        .state
        .context
        .iter()
        .filter_map(|entry| match &entry.kind {
            qaqh_client::ConversationContextKind::Input(input)
                if !consumed.contains(input.input_id.as_str()) =>
            {
                Some((
                    input.input_id.as_str().to_owned(),
                    content_text(&input.content, content_cache),
                ))
            }
            _ => None,
        })
        .collect()
}

/// Apply transcript-bearing conversation deltas. Other UI concerns remain in
/// `App::handle_conversation_delta`.
pub fn apply_delta(session: &mut SessionState, delta: &ConversationDelta) {
    use ConversationDelta as D;
    match delta {
        D::InputAccepted {
            input_id, content, ..
        } => {
            session.pending_input_text.insert(
                input_id.as_str().to_owned(),
                content_text(content, &session.transcript_content),
            );
        }
        D::TurnStarted {
            turn_id, input_id, ..
        } => {
            session
                .current_rounds
                .insert(turn_id.as_str().to_owned(), 0);
            let text = session
                .pending_input_text
                .remove(input_id.as_str())
                .unwrap_or_default();
            apply(
                session,
                turn_id.as_str(),
                TimelineEvent::TurnOpened { user_text: text },
            );
        }
        D::AssistantBlockProgress {
            turn_id,
            block_id,
            block_kind,
            text,
            ..
        } => {
            let kind = match block_kind {
                qaqh_client::fact::AssistantBlockKind::Reasoning => TimelineBlockKind::Reasoning,
                qaqh_client::fact::AssistantBlockKind::Answer => TimelineBlockKind::Text,
                qaqh_client::fact::AssistantBlockKind::ToolCall => return,
            };
            ensure_block(
                session,
                turn_id.as_str(),
                TimelineBlock {
                    block_id: block_id.as_str().to_owned(),
                    block_order: block_order(session, turn_id.as_str(), block_id.as_str()),
                    kind,
                    state: TimelineBlockState::Open,
                    text: String::new(),
                    tool: None,
                    reasoning_kind: None,
                },
            );
            apply(
                session,
                turn_id.as_str(),
                TimelineEvent::BlockCheckpoint {
                    block_id: block_id.as_str().to_owned(),
                    arg: None,
                    text: text.clone(),
                },
            );
        }
        D::AssistantBlockSealed {
            turn_id,
            block_id,
            block_kind,
            content,
            ..
        } => {
            let kind = match block_kind {
                qaqh_client::fact::AssistantBlockKind::Reasoning => TimelineBlockKind::Reasoning,
                qaqh_client::fact::AssistantBlockKind::Answer => TimelineBlockKind::Text,
                qaqh_client::fact::AssistantBlockKind::ToolCall => return,
            };
            let block_id = block_id.as_str().to_owned();
            apply(
                session,
                turn_id.as_str(),
                TimelineEvent::BlockOpened {
                    block: TimelineBlock {
                        block_order: block_order(session, turn_id.as_str(), &block_id),
                        block_id,
                        kind,
                        state: TimelineBlockState::Sealed,
                        text: content_text(content, &session.transcript_content),
                        tool: None,
                        reasoning_kind: None,
                    },
                },
            );
        }
        D::ToolCallDeclared {
            turn_id,
            call_id,
            tool_name,
            args,
            ..
        } => {
            let call_id = call_id.as_str();
            let tool = new_tool(
                call_id,
                tool_name,
                content_text(args, &session.transcript_content),
                TimelineToolState::Running,
            );
            ensure_tool_block(session, turn_id.as_str(), tool, TimelineBlockState::Open);
        }
        D::ToolInputProgress {
            turn_id,
            call_id,
            tool_name,
            args,
            ..
        } => {
            let call_id = call_id.as_str();
            let tool = new_tool(call_id, tool_name, args.clone(), TimelineToolState::Running);
            update_tool(session, turn_id.as_str(), tool);
        }
        D::ToolFinished {
            call_id,
            terminal_status,
            output,
            error,
            metrics,
            ..
        } => {
            if let Some(turn_id) = session.timeline.turns.iter().find_map(|turn| {
                turn.rounds
                    .iter()
                    .flat_map(|round| &round.blocks)
                    .any(|block| block.block_id == call_id.as_str())
                    .then(|| turn.turn_id.clone())
            }) {
                let (name, args_json) = session
                    .timeline
                    .turns
                    .iter()
                    .flat_map(|turn| &turn.rounds)
                    .flat_map(|round| &round.blocks)
                    .find(|block| block.block_id == call_id.as_str())
                    .and_then(|block| block.tool.as_ref())
                    .map(|tool| {
                        (
                            tool.name.clone(),
                            tool.args_json.clone().unwrap_or_default(),
                        )
                    })
                    .unwrap_or_else(|| ("tool".to_owned(), String::new()));
                let failure = error.as_ref().map(|error| TimelineFailure {
                    code: error.code.clone(),
                    message: error.message.clone(),
                });
                let tool = new_tool(
                    call_id.as_str(),
                    &name,
                    args_json,
                    timeline_tool_state(*terminal_status),
                );
                let mut tool = tool;
                tool.output = output
                    .as_ref()
                    .map(|value| content_text(value, &session.transcript_content));
                tool.failure = failure;
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
                update_tool(session, &turn_id, tool);
                apply(
                    session,
                    &turn_id,
                    TimelineEvent::BlockSealed {
                        block_id: call_id.as_str().to_owned(),
                    },
                );
            }
        }
        D::TurnFinished {
            turn_id,
            terminal,
            error,
            ..
        } => {
            let state = match terminal {
                qaqh_client::TurnTerminal::Completed => TimelineTurnState::Completed,
                qaqh_client::TurnTerminal::Failed => TimelineTurnState::Failed,
                qaqh_client::TurnTerminal::Cancelled => TimelineTurnState::Cancelled,
            };
            apply(
                session,
                turn_id.as_str(),
                TimelineEvent::TurnSealed {
                    state,
                    failure: error.as_ref().map(|error| TimelineFailure {
                        code: error.code.clone(),
                        message: error.message.clone(),
                    }),
                },
            );
            session.current_rounds.remove(turn_id.as_str());
        }
        D::TurnInterrupted { turn_id, .. } => {
            apply(
                session,
                turn_id.as_str(),
                TimelineEvent::TurnSealed {
                    state: TimelineTurnState::Cancelled,
                    failure: None,
                },
            );
            session.current_rounds.remove(turn_id.as_str());
        }
        _ => {}
    }
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

pub fn content_ids_from_bootstrap(bootstrap: &ClientV2Bootstrap) -> HashSet<String> {
    let mut ids = HashSet::new();
    for entry in &bootstrap.conversation.state.context {
        match &entry.kind {
            qaqh_client::ConversationContextKind::Input(input) => {
                collect_content_id(&input.content, &mut ids);
            }
            qaqh_client::ConversationContextKind::AssistantBlock(block) => {
                collect_content_id(&block.content, &mut ids);
            }
            qaqh_client::ConversationContextKind::ToolCall(tool) => {
                if let Some(args) = &tool.args {
                    collect_content_id(args, &mut ids);
                }
                if let Some(output) = tool
                    .result
                    .as_ref()
                    .and_then(|result| result.output.as_ref())
                {
                    collect_content_id(output, &mut ids);
                }
            }
            _ => {}
        }
    }
    if let Some(compaction) = &bootstrap.conversation.state.compaction {
        collect_content_id(&compaction.summary, &mut ids);
    }
    ids
}

pub fn content_ids_from_delta(delta: &ConversationDelta) -> HashSet<String> {
    use ConversationDelta as D;
    let mut ids = HashSet::new();
    match delta {
        D::InputAccepted { content, .. }
        | D::AssistantBlockSealed { content, .. }
        | D::ToolCallDeclared { args: content, .. }
        | D::CompactionApplied {
            summary: content, ..
        } => collect_content_id(content, &mut ids),
        D::ToolFinished {
            output: Some(output),
            ..
        } => collect_content_id(output, &mut ids),
        _ => {}
    }
    ids
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

fn turn_outcome(
    outcome: Option<&qaqh_client::v2::projection::ConversationTurnOutcome>,
) -> (TimelineTurnState, Option<TimelineFailure>) {
    match outcome {
        None => (TimelineTurnState::Running, None),
        Some(qaqh_client::v2::projection::ConversationTurnOutcome::Finished {
            terminal,
            error,
            ..
        }) => {
            let state = match terminal {
                qaqh_client::TurnTerminal::Completed => TimelineTurnState::Completed,
                qaqh_client::TurnTerminal::Failed => TimelineTurnState::Failed,
                qaqh_client::TurnTerminal::Cancelled => TimelineTurnState::Cancelled,
            };
            let failure = error.as_ref().map(|error| TimelineFailure {
                code: error.code.clone(),
                message: error.message.clone(),
            });
            (state, failure)
        }
        Some(qaqh_client::v2::projection::ConversationTurnOutcome::Interrupted { .. }) => {
            (TimelineTurnState::Cancelled, None)
        }
    }
}

fn timeline_tool_state(status: qaqh_client::fact::ToolTerminalStatus) -> TimelineToolState {
    use qaqh_client::fact::ToolTerminalStatus as S;
    match status {
        S::Succeeded => TimelineToolState::Succeeded,
        S::Cancelled => TimelineToolState::Cancelled,
        S::Backgrounded => TimelineToolState::Backgrounded,
        S::Failed | S::Partial | S::TimedOut | S::Indeterminate | S::Denied => {
            TimelineToolState::Failed
        }
    }
}

fn tool_metrics(metrics: &qaqh_client::fact::ToolMetrics) -> TimelineToolMetrics {
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

fn tool_block(
    id: &str,
    name: &str,
    args: String,
    state: TimelineToolState,
    output: Option<String>,
    failure: Option<TimelineFailure>,
    metrics: Option<TimelineToolMetrics>,
    order: u32,
) -> TimelineBlock {
    let terminal = state != TimelineToolState::Running && state != TimelineToolState::Prepared;
    let mut tool = new_tool(id, name, args, state);
    tool.output = output.clone();
    tool.failure = failure;
    if let Some(metrics) = metrics {
        tool.display = Some(TimelineToolDisplay {
            summary: None,
            diff: None,
            lines_added: 0,
            lines_removed: 0,
            header: None,
            body: output.map(|text| TimelineToolBody::Text {
                text,
                truncated: false,
            }),
            metrics: Some(metrics),
            outcome: None,
        });
    }
    TimelineBlock {
        block_id: id.to_owned(),
        block_order: order,
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
