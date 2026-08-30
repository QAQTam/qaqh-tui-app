//! 主题：SpanStyle → ratatui Style 的唯一映射。

use ratatui::style::{Color, Modifier, Style};

use crate::app::render_line::SpanStyle;

pub fn style_of(s: SpanStyle) -> Style {
    match s {
        SpanStyle::Plain => Style::new(),
        SpanStyle::Dim => Style::new().fg(Color::DarkGray),
        SpanStyle::Reasoning => Style::new().fg(Color::DarkGray).add_modifier(Modifier::ITALIC),
        SpanStyle::Italic => Style::new().add_modifier(Modifier::ITALIC),
        SpanStyle::User => Style::new().fg(Color::White).add_modifier(Modifier::BOLD),
        SpanStyle::Accent => Style::new().fg(Color::Cyan),
        SpanStyle::ToolRun => Style::new().fg(Color::Yellow),
        SpanStyle::ToolOk => Style::new().fg(Color::Green),
        SpanStyle::ToolFail => Style::new().fg(Color::Red),
        SpanStyle::Warn => Style::new().fg(Color::Yellow),
        SpanStyle::Error => Style::new().fg(Color::Red).add_modifier(Modifier::BOLD),
        SpanStyle::DiffAdd => Style::new().fg(Color::Green),
        SpanStyle::DiffDel => Style::new().fg(Color::Red),
        SpanStyle::Inverse => Style::new().add_modifier(Modifier::REVERSED),
    }
}

pub fn accent() -> Style {
    Style::new().fg(Color::Cyan)
}

pub fn dim() -> Style {
    Style::new().fg(Color::DarkGray)
}

pub fn active_tab() -> Style {
    Style::new().fg(Color::Black).bg(Color::Cyan).add_modifier(Modifier::BOLD)
}

pub fn modal_border() -> Style {
    Style::new().fg(Color::Yellow)
}

pub fn ok() -> Style {
    Style::new().fg(Color::Green)
}

pub fn err() -> Style {
    Style::new().fg(Color::Red)
}

pub fn warn() -> Style {
    Style::new().fg(Color::Yellow)
}
