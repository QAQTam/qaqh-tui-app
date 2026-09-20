//! V2 inline 原型（M1）。
//!
//! 运行：`qaqh-tui --v2-inline`
//!
//! 该原型刻意不连接 daemon、不进入 alternate screen，只验证三件事：
//! 1. `Viewport::Inline` 能承载底部 live viewport；
//! 2. Enter 可把已封口文本 `insert_before` 到终端 scrollback；
//! 3. 同一 `CommitId` 重放不会重复提交。
//!
//! 这不是最终 v2 UI；默认 v1 路径不受影响。

use anyhow::Result;
use ratatui::crossterm::event::{Event, KeyCode, KeyEventKind, KeyModifiers, read};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Widget};
use ratatui::{DefaultTerminal, Frame, TerminalOptions, Viewport};

use crate::terminal::commit::{CommitDecision, CommitId, CommitLedger, content_hash};

const VIEWPORT_HEIGHT: u16 = 7;

/// 启动隔离的 inline 原型。
pub fn run_prototype() -> Result<()> {
    let mut terminal = ratatui::init_with_options(TerminalOptions {
        viewport: Viewport::Inline(VIEWPORT_HEIGHT),
    });
    let result = run_loop(&mut terminal);
    ratatui::restore();
    result
}

struct PrototypeState {
    live: String,
    next_seq: u64,
    last_commit: Option<(CommitId, String)>,
    ledger: CommitLedger,
    status: String,
}

impl Default for PrototypeState {
    fn default() -> Self {
        Self {
            live: String::new(),
            next_seq: 0,
            last_commit: None,
            ledger: CommitLedger::new(),
            status: "ready · Enter commit · r replay · q quit".to_string(),
        }
    }
}

fn run_loop(terminal: &mut DefaultTerminal) -> Result<()> {
    let mut state = PrototypeState::default();
    loop {
        terminal.draw(|f| draw(f, &state))?;
        let key = match read()? {
            Event::Key(key) if key.kind == KeyEventKind::Press => key,
            Event::Resize(_, _) => {
                state.status = "resized · redrawing".to_string();
                continue;
            }
            _ => continue,
        };
        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => break,
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => break,
            KeyCode::Enter => commit_live(terminal, &mut state)?,
            KeyCode::Char('r') => replay_last(&mut state),
            KeyCode::Backspace => {
                state.live.pop();
            }
            KeyCode::Char(c) => state.live.push(c),
            _ => {}
        }
    }
    Ok(())
}

fn commit_live(terminal: &mut DefaultTerminal, state: &mut PrototypeState) -> Result<()> {
    let text = state.live.trim().to_string();
    if text.is_empty() {
        state.status = "empty input · nothing committed".to_string();
        return Ok(());
    }

    let seq = state.next_seq;
    let id = CommitId::new("prototype", "turn-0", format!("block-{seq}"), 1);
    let hash = content_hash(&text);
    match state.ledger.commit(id.clone(), hash) {
        CommitDecision::Emit => {
            let line = committed_line(seq, &text);
            terminal.insert_before(1, |buf| {
                line.render(buf.area, buf);
            })?;
            state.last_commit = Some((id, text));
            state.next_seq += 1;
            state.status = format!("committed block-{seq}");
        }
        CommitDecision::Duplicate => {
            state.status = format!("skipped duplicate block-{seq}");
        }
        CommitDecision::Conflict => {
            state.status = format!("conflict on block-{seq}; kept original");
        }
    }
    state.live.clear();
    Ok(())
}

fn replay_last(state: &mut PrototypeState) {
    let Some((id, text)) = state.last_commit.clone() else {
        state.status = "no commit to replay".to_string();
        return;
    };
    state.status = match state.ledger.commit(id, content_hash(&text)) {
        CommitDecision::Duplicate => "replay skipped · duplicate commit id".to_string(),
        CommitDecision::Conflict => "replay rejected · content changed".to_string(),
        CommitDecision::Emit => "replay unexpectedly emitted".to_string(),
    };
}

fn committed_line(seq: u64, text: &str) -> Line<'static> {
    Line::from(vec![
        Span::styled("  ❯ ", Style::new().fg(Color::Cyan)),
        Span::styled(format!("#{seq} "), Style::new().fg(Color::DarkGray)),
        Span::raw(text.to_string()),
    ])
}

fn draw(f: &mut Frame, state: &PrototypeState) {
    let dim = Style::new().fg(Color::DarkGray);
    let lines = vec![
        Line::from(vec![
            Span::styled(
                "QAQH v2 inline prototype",
                Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD),
            ),
            Span::styled("  · M1", dim),
        ]),
        Line::from(vec![
            Span::styled("live ❯ ", Style::new().fg(Color::Magenta)),
            Span::raw(state.live.clone()),
            Span::styled("▌", Style::new().fg(Color::Magenta)),
        ]),
        Line::from(Span::styled(state.status.clone(), dim)),
        Line::from(Span::styled(
            "Enter commit · r replay last · q quit",
            Style::new().fg(Color::DarkGray),
        )),
    ];
    let block = Block::new()
        .borders(Borders::TOP | Borders::BOTTOM)
        .border_style(dim);
    f.render_widget(Paragraph::new(lines).block(block), f.area());
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::layout::Rect;

    #[test]
    fn committed_line_contains_sequence_and_text() {
        let line = committed_line(3, "hello");
        let text: String = line
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect();
        assert!(text.contains("#3"), "{text}");
        assert!(text.contains("hello"), "{text}");
    }

    #[test]
    fn replay_uses_same_identity_and_is_duplicate() {
        let mut state = PrototypeState {
            live: "hello".to_string(),
            ..Default::default()
        };
        let id = CommitId::new("prototype", "turn-0", "block-0", 1);
        assert_eq!(
            state.ledger.commit(id.clone(), content_hash("hello")),
            CommitDecision::Emit
        );
        state.last_commit = Some((id, "hello".to_string()));
        replay_last(&mut state);
        assert!(state.status.contains("duplicate"), "{}", state.status);
    }

    #[test]
    fn inline_viewport_survives_resize() {
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::with_options(
            backend,
            TerminalOptions {
                viewport: Viewport::Inline(VIEWPORT_HEIGHT),
            },
        )
        .expect("inline terminal");

        terminal
            .draw(|f| draw(f, &PrototypeState::default()))
            .expect("first draw");
        terminal.resize(Rect::new(0, 0, 100, 30)).expect("resize");
        terminal
            .draw(|f| draw(f, &PrototypeState::default()))
            .expect("draw after resize");
    }
}
