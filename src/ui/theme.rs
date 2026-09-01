//! 主题：SpanStyle → ratatui Style 的唯一映射。

use ratatui::style::{Color, Modifier, Style};

use crate::app::render_line::SpanStyle;

pub fn style_of(s: SpanStyle) -> Style {
    match s {
        SpanStyle::Plain => Style::new(),
        SpanStyle::Dim => Style::new().fg(Color::DarkGray),
        // F3 思考链：橙色+斜体，与工具返回的 Dim(DarkGray) 拉开区分度
        // 绿色会与 ToolOk/DiffAdd 撞色，故选用橙色 208/Rgb(255,165,0)
        SpanStyle::Reasoning => Style::new().fg(Color::Rgb(255, 165, 0)).add_modifier(Modifier::ITALIC),
        // Emphasis 在 markdown 中映射为 Italic：Yellow+Italic，纯 SGR3 在 konsole 不可见
        SpanStyle::Italic => Style::new().fg(Color::Yellow).add_modifier(Modifier::ITALIC),
        SpanStyle::Bold => Style::new().add_modifier(Modifier::BOLD),
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
        SpanStyle::MdH1 => Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD),
        SpanStyle::MdH2 => Style::new().fg(Color::LightCyan).add_modifier(Modifier::BOLD),
        SpanStyle::MdH3 => Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD | Modifier::UNDERLINED),
        SpanStyle::MdInlineCode => Style::new().fg(Color::Yellow).bg(Color::Indexed(236)),
        SpanStyle::MdCodeBlock => Style::new().fg(Color::White).bg(Color::Indexed(236)),
        SpanStyle::MdLink => Style::new().fg(Color::LightBlue).add_modifier(Modifier::UNDERLINED),
        SpanStyle::MdQuote => Style::new().fg(Color::DarkGray).add_modifier(Modifier::ITALIC),
        SpanStyle::MdRuler => Style::new().fg(Color::DarkGray),
        SpanStyle::MdTableHead => Style::new().fg(Color::Black).bg(Color::Cyan).add_modifier(Modifier::BOLD),
        SpanStyle::MdTableCell => Style::new(),
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
