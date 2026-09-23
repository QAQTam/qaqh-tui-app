//! V2 inline 原型（M1）。
//!
//! 运行：`qaqh-tui --v2-inline`
//!
//! 该原型刻意不连接 daemon、不进入 alternate screen，只验证三件事：
//! 1. `Viewport::Inline` 能承载底部 live viewport；
//! 2. Enter 可把已封口文本 `insert_before` 到终端 scrollback；
//! 3. 同一 `CommitId` 重放不会重复提交。
//!
//! 这不是最终 v2 UI；默认 Agent View 不依赖本原型，`--v1` 回退路径也不受影响。

use anyhow::Result;
use ratatui::crossterm::event::{Event, KeyCode, KeyEventKind, KeyModifiers, read};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Widget};
use ratatui::{DefaultTerminal, Frame, TerminalOptions, Viewport};

use crate::terminal::commit::CommitDecision;
use crate::terminal::transcript::TranscriptCommitLedger;
use crate::theme::Theme;
use crate::ui::v2::transcript::{BlockKind, BlockState, TranscriptBlock, render_block};

const VIEWPORT_HEIGHT: u16 = 7;
/// 原型阶段先给 live buffer 一个明确上限，避免异常输入把 TUI 内存拖垮。
const MAX_LIVE_BYTES: usize = 64 * 1024;

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
    last_commit: Option<TranscriptBlock>,
    ledger: TranscriptCommitLedger,
    status: String,
}

impl Default for PrototypeState {
    fn default() -> Self {
        Self {
            live: String::new(),
            next_seq: 0,
            last_commit: None,
            ledger: TranscriptCommitLedger::new(),
            status: "ready · Enter commit · r replay · q quit".to_string(),
        }
    }
}

fn run_loop(terminal: &mut DefaultTerminal) -> Result<()> {
    let mut state = PrototypeState::default();
    let theme = Theme::current();
    loop {
        terminal.draw(|f| draw(f, &state, theme))?;
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
            KeyCode::Enter => commit_live(terminal, &mut state, theme)?,
            KeyCode::Char('r') => replay_last(&mut state),
            KeyCode::Backspace => {
                state.live.pop();
            }
            KeyCode::Char(c) => push_live(&mut state, c),
            _ => {}
        }
    }
    Ok(())
}

fn push_live(state: &mut PrototypeState, c: char) {
    if state.live.len().saturating_add(c.len_utf8()) > MAX_LIVE_BYTES {
        state.status = format!("input limit {MAX_LIVE_BYTES} bytes reached");
        return;
    }
    state.live.push(c);
}

fn commit_live(
    terminal: &mut DefaultTerminal,
    state: &mut PrototypeState,
    theme: &Theme,
) -> Result<()> {
    let text = state.live.trim().to_string();
    if text.is_empty() {
        state.status = "empty input · nothing committed".to_string();
        return Ok(());
    }

    let seq = state.next_seq;
    let mut block = TranscriptBlock::new(
        format!("block-{seq}"),
        BlockKind::User { text: text.clone() },
    )
    .with_turn_id("turn-0");
    block.seal();
    match state.ledger.commit_block("prototype", &mut block) {
        Some(CommitDecision::Emit) => {
            let width = terminal.get_frame().area().width;
            let lines = render_block(&block, usize::from(width), theme);
            let height = u16::try_from(lines.len().max(1)).unwrap_or(u16::MAX);
            terminal.insert_before(height, |buf| {
                Paragraph::new(lines).render(buf.area, buf);
            })?;
            state.last_commit = Some(block);
            state.next_seq += 1;
            state.status = format!("committed block-{seq}");
        }
        Some(CommitDecision::Duplicate) => {
            state.status = format!("skipped duplicate block-{seq}");
        }
        Some(CommitDecision::Conflict) => {
            state.status = format!("conflict on block-{seq}; kept original");
        }
        None => state.status = "block was not sealed".to_string(),
    }
    state.live.clear();
    Ok(())
}

fn replay_last(state: &mut PrototypeState) {
    let Some(mut block) = state.last_commit.clone() else {
        state.status = "no commit to replay".to_string();
        return;
    };
    block.state = BlockState::Sealed;
    state.status = match state.ledger.commit_block("prototype", &mut block) {
        Some(CommitDecision::Duplicate) => "replay skipped · duplicate commit id".to_string(),
        Some(CommitDecision::Conflict) => "replay rejected · content changed".to_string(),
        Some(CommitDecision::Emit) => "replay unexpectedly emitted".to_string(),
        None => "replay rejected · block was not sealed".to_string(),
    };
}

fn draw(f: &mut Frame, state: &PrototypeState, theme: &Theme) {
    let dim = Style::new().fg(theme.text.dim);
    let lines = vec![
        Line::from(vec![
            Span::styled(
                "QAQH v2 inline prototype",
                Style::new()
                    .fg(theme.accent.system)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(format!("  {} M1", theme.glyph.system), dim),
        ]),
        Line::from(vec![
            Span::styled(
                format!("live {} ", theme.glyph.user),
                Style::new().fg(theme.accent.user),
            ),
            Span::styled(state.live.as_str(), Style::new().fg(theme.text.primary)),
            Span::styled(theme.glyph.cursor, Style::new().fg(theme.accent.user)),
        ]),
        Line::from(Span::styled(state.status.as_str(), dim)),
        Line::from(Span::styled("Enter commit · r replay last · q quit", dim)),
    ];
    let block = Block::new()
        .borders(Borders::TOP | Borders::BOTTOM)
        .border_style(dim);
    f.render_widget(Paragraph::new(lines).block(block), f.area());
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme::{ColorSupport, ThemeKind};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::layout::Rect;

    fn test_theme() -> Theme {
        Theme::resolve(ThemeKind::QaqhNight, ColorSupport::TrueColor)
    }

    #[test]
    fn committed_block_contains_prompt_and_text() {
        let theme = test_theme();
        let block = TranscriptBlock::new(
            "block-3",
            BlockKind::User {
                text: "hello".to_string(),
            },
        );
        let lines = render_block(&block, 40, &theme);
        let text: String = lines
            .iter()
            .flat_map(|line| line.spans.iter())
            .map(|span| span.content.as_ref())
            .collect();
        assert!(text.contains('❯'), "{text}");
        assert!(text.contains("hello"), "{text}");
    }

    #[test]
    fn committed_block_uses_theme_tokens() {
        let theme = test_theme();
        let block = TranscriptBlock::new(
            "block-3",
            BlockKind::User {
                text: "hello".to_string(),
            },
        );
        let lines = render_block(&block, 40, &theme);
        assert_eq!(lines[0].spans[0].style.fg, Some(theme.accent.user));
        assert_eq!(lines[0].spans[1].style.fg, Some(theme.text.primary));
    }

    #[test]
    fn replay_uses_same_identity_and_is_duplicate() {
        let mut state = PrototypeState {
            live: "hello".to_string(),
            ..Default::default()
        };
        let mut block = TranscriptBlock::new(
            "block-0",
            BlockKind::User {
                text: "hello".to_string(),
            },
        )
        .with_turn_id("turn-0");
        block.seal();
        assert_eq!(
            state.ledger.commit_block("prototype", &mut block),
            Some(CommitDecision::Emit)
        );
        state.last_commit = Some(block);
        replay_last(&mut state);
        assert!(state.status.contains("duplicate"), "{}", state.status);
    }

    #[test]
    fn live_input_is_bounded() {
        let mut state = PrototypeState {
            live: "a".repeat(MAX_LIVE_BYTES),
            ..Default::default()
        };
        push_live(&mut state, 'b');
        assert_eq!(state.live.len(), MAX_LIVE_BYTES);
        assert!(state.status.contains("input limit"), "{}", state.status);
    }

    #[test]
    fn inline_viewport_survives_repeated_resize() {
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::with_options(
            backend,
            TerminalOptions {
                viewport: Viewport::Inline(VIEWPORT_HEIGHT),
            },
        )
        .expect("inline terminal");
        let theme = test_theme();

        for (width, height) in [(80, 24), (100, 30), (60, 18), (120, 40)] {
            terminal
                .resize(Rect::new(0, 0, width, height))
                .expect("resize");
            terminal
                .draw(|f| draw(f, &PrototypeState::default(), &theme))
                .expect("draw after resize");
        }
    }
}
