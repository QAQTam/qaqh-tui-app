//! V2 blocking modal 渲染。
//!
//! permission / ask / plan 与确认、路径输入、思考回放统一走 alternate-screen
//! Modal。ask 采用 Grok 式单题分页：一页一个问题，左右切题，上下选选项，
//! Enter 选择并前进，Space 只选择不前进。

use ratatui::Frame;
use ratatui::crossterm::event::MouseButton;
use ratatui::layout::{Constraint, Layout, Position, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};
use unicode_width::UnicodeWidthStr;

use crate::app::session::{AskPanel, PermissionPanel, PlanPanel, option_shortcut_label};
use crate::app::{App, ConfirmAction, ModalHit, Overlay};
use crate::theme::Theme;
use crate::ui::v2::button::{ButtonState, ButtonVisual};
use crate::ui::v2::hit::{
    HitMapBuilder, PointerTarget, VisualAnchor, anchor_region, line_region, z,
};
use crate::ui::v2::route::ModalRoute;

// ───────────────────────── 按钮：状态 → 样式 ─────────────────────────
//
// 鼠标交互的**视觉**只有两档：悬停（底色抬一档）与按下（底色再压一档 + 加粗）。
// 刻意不做动画：终端一帧就是一格一色，做不出平滑过渡，硬做只会闪。
//
// 降级：`terminal` 主题里 `surface.*` 全是 `Color::Reset`（没有底色），只靠背景
// 色会完全看不出反馈，所以**按下额外加 BOLD**——那是该主题下仍可见的最小信号。
// 键盘焦点另走 `▶` 前缀（一直都有），不依赖颜色。

/// 一个按钮的视觉状态由全仓统一的 [`ButtonVisual`] 推导（spec §5.1）。
///
/// 这里只保留弹窗自己的**指针语义状态**；颜色/优先级规则一律在
/// `ui::v2::button` 里，不允许本模块再定义一套。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct MouseState {
    pub hover: Option<ModalHit>,
    pub pressed: Option<ModalHit>,
}

impl MouseState {
    pub fn from_app(app: &App) -> Self {
        Self {
            hover: app.modal_hover,
            pressed: app.modal_pressed,
        }
    }

    /// 某个目标在这一帧的视觉。Modal 的键盘焦点由 `▶` 前缀表达，不靠底色，
    /// 所以 `focused = false`。
    pub fn visual(self, target: ModalHit) -> ButtonVisual {
        ButtonVisual::derive(
            true,
            false,
            self.hover == Some(target),
            self.pressed == Some(target),
        )
    }
}

/// 弹窗按钮的底色：`▶` 前缀已经表达焦点，不需要额外的焦点底色。
fn button_style(visual: ButtonVisual, theme: &Theme) -> Style {
    visual.surface_style(theme, Color::Reset)
}

/// 把整行刷上按钮底色并补空格铺满 `width`。
///
/// 按钮的"块感"来自**整行同底色**；只给文字上色会变成高亮字，不是按钮。
fn button_line(
    spans: Vec<Span<'static>>,
    width: usize,
    visual: ButtonVisual,
    theme: &Theme,
) -> Line<'static> {
    let base = button_style(visual, theme);
    let mut spans: Vec<Span<'static>> = spans
        .into_iter()
        .map(|span| span.patch_style(base))
        .collect();
    let used: usize = spans.iter().map(|span| span.width()).sum();
    if used < width {
        spans.push(Span::styled(" ".repeat(width - used), base));
    }
    Line::from(spans)
}

/// 横排按钮的布局：`(矩形, 目标)`。**渲染与命中测试共用**，避免两边各算一次
/// 导致"看着在这里、点着在那里"。
///
/// 每个按钮宽度按 `" {label} "` 算（左右各一格 padding），按钮之间空一格。
fn button_row_rects(specs: &[(&str, ModalHit)], area: Rect) -> Vec<(Rect, ModalHit)> {
    let mut x = area.x;
    let mut out = Vec::with_capacity(specs.len());
    for (label, target) in specs {
        let width = (label.width() as u16).saturating_add(2);
        if x.saturating_add(width) > area.x.saturating_add(area.width) {
            break;
        }
        out.push((
            Rect {
                x,
                y: area.y,
                width,
                height: 1,
            },
            *target,
        ));
        x = x.saturating_add(width).saturating_add(1);
    }
    out
}

/// 画一排按钮，并**同时**把每个按钮登记进当前帧的 HitMap。
///
/// 按下态把文字整体右移一格（右侧被裁掉一格），用**字形位移**模拟下沉——这是
/// 终端里最接近"按下去"的做法。按钮矩形来自 `button_row_rects`，登记用的也是
/// 同一份返回结果，所以"画在哪里"和"能点哪里"不可能各算一套。
fn draw_button_row(
    f: &mut Frame,
    specs: &[(&str, ModalHit)],
    area: Rect,
    clip: Rect,
    theme: &Theme,
    mouse: MouseState,
    hit_map: &mut HitMapBuilder,
) {
    for ((label, target), (rect, _)) in specs.iter().zip(button_row_rects(specs, area)) {
        let visual = mouse.visual(*target);
        // 按下态把文字整体右移一格（右侧被裁掉一格），用字形位移模拟下沉。
        let text = if visual.state() == ButtonState::Pressed {
            format!("  {label} ")
        } else {
            format!(" {label} ")
        };
        let line = button_line(
            vec![Span::styled(text, Style::new().fg(theme.text.primary))],
            usize::from(rect.width),
            visual,
            theme,
        );
        register_modal_region(hit_map, rect, clip, *target, z::MODAL_BUTTON, &line);
        f.render_widget(Paragraph::new(line), rect);
    }
}

/// 登记整屏 Modal 阻断层：点在弹窗外命中 `ModalRoot`，不得穿透到底层。
///
/// `dialog` 是弹窗外框矩形，锚点取它的左上角——`Block` 一定在那里画边框字形，
/// P0-C 的 strict probe 可以在真实 buffer 上验证这个锚点非空。
fn register_modal_root(hit_map: &mut HitMapBuilder, area: Rect, dialog: Rect) {
    if let Some(region) = anchor_region(
        area,
        area,
        PointerTarget::ModalRoot,
        MouseButton::Left,
        true,
        z::MODAL_ROOT,
        VisualAnchor::non_empty(Position::new(dialog.x, dialog.y)),
    ) {
        hit_map.push(region);
    }
}

/// 把一个弹窗语义目标登记进当前帧的 HitMap。
///
/// `rect` / `clip` 必须是绘制阶段正在使用的屏幕绝对坐标；`line` 是这一行真正
/// 渲染的内容，锚点由 [`line_region`] 从它推导。
fn register_modal_region(
    hit_map: &mut HitMapBuilder,
    rect: Rect,
    clip: Rect,
    target: ModalHit,
    z: u16,
    line: &Line<'_>,
) {
    if let Some(region) = line_region(
        rect,
        clip,
        PointerTarget::Modal(target),
        MouseButton::Left,
        true,
        z,
        line,
    ) {
        hit_map.push(region);
    }
}

/// 弹窗里一行的内容 + 它对应的可点目标（`None` = 纯文本）。
struct ModalRow {
    line: Line<'static>,
    target: Option<ModalHit>,
}

// 三个弹窗的外框。**draw 与 hit_test 共用**——各算一次就迟早错位。
fn ask_rect(area: Rect) -> Rect {
    centered_rect(
        88u16.min(area.width.saturating_sub(4)),
        area.height.saturating_sub(4).min(30),
        area,
    )
}

fn permission_rect(area: Rect) -> Rect {
    centered_rect(
        72u16.min(area.width.saturating_sub(4)),
        22u16.min(area.height.saturating_sub(4)),
        area,
    )
}

fn plan_rect(area: Rect) -> Rect {
    centered_rect(
        88u16.min(area.width.saturating_sub(4)),
        area.height.saturating_sub(4),
        area,
    )
}

/// Modal 的命中不再有独立入口：`draw` 在真实 render 位置把按钮/选项登记进
/// `HitMapBuilder`（P0-B-4 删掉了旧的 `modal::hit_test`），事件只查已发布的
/// `FrameHitMap`。
pub fn draw(
    f: &mut Frame,
    app: &App,
    area: Rect,
    theme: &Theme,
    route: ModalRoute,
    hit_map: &mut HitMapBuilder,
) -> bool {
    match route {
        ModalRoute::Permission => {
            let Some(permission) = app
                .active_session()
                .and_then(|session| session.active_permission())
            else {
                return false;
            };
            draw_permission(
                f,
                permission,
                area,
                theme,
                MouseState::from_app(app),
                hit_map,
            );
        }
        ModalRoute::Ask => {
            let Some(ask) = app
                .active_session()
                .and_then(|session| session.pending_ask.as_ref())
            else {
                return false;
            };
            draw_ask(f, ask, area, theme, MouseState::from_app(app), hit_map);
        }
        ModalRoute::Plan => {
            let Some(plan) = app
                .active_session()
                .and_then(|session| session.pending_plan.as_ref())
            else {
                return false;
            };
            draw_plan(f, plan, area, theme, MouseState::from_app(app), hit_map);
        }
        ModalRoute::Confirm => {
            let Some(Overlay::Confirm { action }) = app.overlays.last() else {
                return false;
            };
            draw_confirm(f, action, area, theme, hit_map);
        }
        ModalRoute::AttachPath => {
            let Some(Overlay::AttachPath { input, cursor, .. }) = app.overlays.last() else {
                return false;
            };
            draw_input(
                f,
                input,
                *cursor,
                area,
                theme,
                InputSpec {
                    title: "附件路径",
                    prefix: "路径> ",
                    keys: &[("Enter", "上传"), ("Esc", "取消")],
                },
                hit_map,
            );
        }
        ModalRoute::CwdInput => {
            let Some(Overlay::CwdInput { input, cursor }) = app.overlays.last() else {
                return false;
            };
            draw_input(
                f,
                input,
                *cursor,
                area,
                theme,
                InputSpec {
                    title: "新建会话目录",
                    prefix: "cwd> ",
                    keys: &[("Enter", "确认"), ("Esc", "取消")],
                },
                hit_map,
            );
        }
        ModalRoute::Thinking => {
            let Some(Overlay::Thinking { scroll, body, .. }) = app.overlays.last() else {
                return false;
            };
            draw_thinking(f, *scroll, body, area, theme, hit_map);
        }
    }
    true
}

/// permission 面板的行布局（渲染与命中测试共用，同 `ask_rows`）。
fn permission_rows(
    panel: &PermissionPanel,
    inner_width: usize,
    theme: &Theme,
    mouse: MouseState,
) -> Vec<ModalRow> {
    let mut rows: Vec<ModalRow> = Vec::new();
    let push = |rows: &mut Vec<ModalRow>, prefix: &str, text: &str, style: Style| {
        let mut lines = Vec::new();
        push_wrapped(&mut lines, prefix, text, inner_width, style);
        rows.extend(
            lines
                .into_iter()
                .map(|line| ModalRow { line, target: None }),
        );
    };
    push(
        &mut rows,
        "工具: ",
        &panel.tool_name,
        Style::new().fg(theme.accent.tool),
    );
    if let Some(action) = panel.action_summary.as_deref() {
        push(
            &mut rows,
            "执行: ",
            action,
            Style::new().fg(theme.semantic.command),
        );
    }
    if !panel.reason.is_empty() {
        push(
            &mut rows,
            "原因: ",
            &panel.reason,
            Style::new().fg(theme.text.secondary),
        );
    }
    push(
        &mut rows,
        "类别: ",
        &format!("{:?}（影响等级 {}）", panel.category, panel.level),
        Style::new().fg(theme.text.secondary),
    );
    if !panel.consequence.is_empty() {
        push(
            &mut rows,
            "后果: ",
            &panel.consequence,
            Style::new().fg(theme.text.secondary),
        );
    }
    for path in panel.paths.iter().take(6) {
        push(
            &mut rows,
            "路径: ",
            path,
            Style::new().fg(theme.semantic.path),
        );
    }
    rows.push(ModalRow {
        line: Line::default(),
        target: None,
    });
    // 「信任此目录」做成可点行：它本来就是二态开关，鼠标点它比记 `t` 自然。
    let target = ModalHit::PermissionTrust;
    let trust = if panel.trust_folder { "[x]" } else { "[ ]" };
    let spans = vec![
        Span::styled(
            format!(" {trust} 信任此目录"),
            Style::new().fg(if panel.trust_folder {
                theme.accent.success
            } else {
                theme.text.dim
            }),
        ),
        Span::styled(" · t 切换", Style::new().fg(theme.text.dim)),
    ];
    rows.push(ModalRow {
        line: button_line(spans, inner_width, mouse.visual(target), theme),
        target: Some(target),
    });
    rows
}

fn draw_permission(
    f: &mut Frame,
    panel: &PermissionPanel,
    area: Rect,
    theme: &Theme,
    mouse: MouseState,
    hit_map: &mut HitMapBuilder,
) {
    let rect = permission_rect(area);
    if rect.width < 8 || rect.height < 5 {
        return;
    }
    register_modal_root(hit_map, area, rect);
    f.render_widget(Clear, rect);
    f.render_widget(
        Block::new()
            .borders(Borders::ALL)
            .border_style(Style::new().fg(theme.semantic.warning))
            .title(format!(
                " ⚠ 工具权限 · {:?} · level {} ",
                panel.risk, panel.level
            )),
        rect,
    );
    let inner = inner_rect(rect);
    let [content_area, footer_area] =
        Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).areas(inner);
    let rows = permission_rows(panel, usize::from(inner.width), theme, mouse);
    // 正文行没有滚动，第 `index` 行就落在 `content_area.y + index`；超出可见高度的
    // 行会被 Paragraph 裁掉，所以只登记真正画出来的那一段。
    for (index, row) in rows
        .iter()
        .take(usize::from(content_area.height))
        .enumerate()
    {
        let Some(target) = row.target else {
            continue;
        };
        let row_rect = Rect {
            x: content_area.x,
            y: content_area.y.saturating_add(index as u16),
            width: content_area.width,
            height: 1,
        };
        register_modal_region(
            hit_map,
            row_rect,
            content_area,
            target,
            z::MODAL_CONTENT,
            &row.line,
        );
    }
    f.render_widget(
        Paragraph::new(rows.into_iter().map(|row| row.line).collect::<Vec<_>>())
            .wrap(Wrap { trim: false }),
        content_area,
    );
    draw_button_row(
        f,
        &[
            ("a 批准", ModalHit::PermissionApprove),
            ("d/Esc 拒绝", ModalHit::PermissionDeny),
        ],
        footer_area,
        inner,
        theme,
        mouse,
        hit_map,
    );
}

fn draw_plan(
    f: &mut Frame,
    panel: &PlanPanel,
    area: Rect,
    theme: &Theme,
    mouse: MouseState,
    hit_map: &mut HitMapBuilder,
) {
    let rect = plan_rect(area);
    if rect.width < 8 || rect.height < 5 {
        return;
    }
    register_modal_root(hit_map, area, rect);
    f.render_widget(Clear, rect);
    let review_type = if panel.review_type.is_empty() {
        "plan"
    } else {
        panel.review_type.as_str()
    };
    f.render_widget(
        Block::new()
            .borders(Borders::ALL)
            .border_style(Style::new().fg(theme.semantic.plan))
            .title(format!(" 📋 计划评审 · {review_type} ")),
        rect,
    );
    let inner = inner_rect(rect);
    let footer_height = if panel.entering_message { 2 } else { 1 };
    let [content_area, footer_area] =
        Layout::vertical([Constraint::Min(1), Constraint::Length(footer_height)]).areas(inner);
    let content_width = usize::from(inner.width);
    let wrapped = crate::app::render_line::wrap_text(&panel.plan_content, content_width);
    let start = panel.scroll.min(wrapped.len().saturating_sub(1));
    let available = usize::from(content_area.height);
    let mut lines: Vec<Line<'static>> = wrapped
        .into_iter()
        .skip(start)
        .take(available)
        .map(|line| Line::from(Span::styled(line, Style::new().fg(theme.text.primary))))
        .collect();
    if !panel.todo_items.is_empty() {
        lines.push(Line::default());
        lines.push(Line::from(Span::styled(
            "Todo",
            Style::new()
                .fg(theme.accent.assistant)
                .add_modifier(Modifier::BOLD),
        )));
        for item in &panel.todo_items {
            lines.push(Line::from(vec![
                Span::styled(
                    // `complexity` 是 String（"small"|"medium"|"large"），`{:?}` 会渲染成
                    // 带引号的 `"small"`——后端交底 §3 专门点了这处。
                    format!("  [{}] ", item.complexity),
                    Style::new().fg(theme.text.dim),
                ),
                Span::styled(item.title.clone(), Style::new().fg(theme.text.secondary)),
            ]));
        }
    }
    lines.truncate(available);
    f.render_widget(
        Paragraph::new(lines).wrap(Wrap { trim: false }),
        content_area,
    );

    if panel.entering_message {
        // 填理由时 footer 是输入态，不是按钮行。
        let mut footer_lines = Vec::with_capacity(usize::from(footer_height));
        footer_lines.push(Line::from(vec![
            Span::styled("  拒绝理由> ", Style::new().fg(theme.semantic.warning)),
            Span::styled(
                format!("{}_", panel.message),
                Style::new()
                    .fg(theme.text.primary)
                    .add_modifier(Modifier::REVERSED),
            ),
        ]));
        footer_lines.push(footer(&[("Enter", "提交拒绝"), ("Esc", "取消输入")], theme));
        f.render_widget(Paragraph::new(footer_lines), footer_area);
    } else {
        let specs = [
            ("a 批准", ModalHit::PlanApprove),
            ("g 批准+自主", ModalHit::PlanApproveAutonomous),
            ("r 拒绝并填写理由", ModalHit::PlanReject),
        ];
        let rects = button_row_rects(&specs, footer_area);
        draw_button_row(f, &specs, footer_area, inner, theme, mouse, hit_map);
        // 滚动提示留在按钮右边（不可点）。
        let used_end = rects
            .last()
            .map(|(rect, _)| rect.x.saturating_add(rect.width).saturating_add(1))
            .unwrap_or(footer_area.x);
        let right = footer_area.x.saturating_add(footer_area.width);
        if used_end < right {
            f.render_widget(
                Paragraph::new(footer(&[("↑↓/PgUp/PgDn", "滚动")], theme)),
                Rect {
                    x: used_end,
                    y: footer_area.y,
                    width: right.saturating_sub(used_end),
                    height: 1,
                },
            );
        }
    }
    if panel.entering_message {
        let y = footer_area.y;
        let x = inner
            .x
            .saturating_add("  拒绝理由> ".width() as u16)
            .saturating_add(panel.message.width() as u16);
        if x < inner.x.saturating_add(inner.width) {
            f.set_cursor_position((x, y));
        }
    }
}

fn draw_confirm(
    f: &mut Frame,
    action: &ConfirmAction,
    area: Rect,
    theme: &Theme,
    hit_map: &mut HitMapBuilder,
) {
    let (title, body) = match action {
        ConfirmAction::DeleteSession(seed) => (
            "确认删除",
            format!("彻底删除会话 {seed}？磁盘数据不可恢复。"),
        ),
        ConfirmAction::ArchiveSession(seed) => ("确认归档", format!("归档会话 {seed}？")),
        ConfirmAction::CloseTab(seed) => {
            ("确认关闭", format!("关闭标签 {seed}？会话仍保留在列表中。"))
        }
        ConfirmAction::UndoTurn { turn_id, .. } => (
            "确认撤销",
            format!("撤销回合 {turn_id} 及其后的全部对话？工具副作用不会回滚。"),
        ),
    };
    let rect = centered_rect(64u16.min(area.width.saturating_sub(4)), 8, area);
    if rect.width < 8 || rect.height < 5 {
        return;
    }
    register_modal_root(hit_map, area, rect);
    f.render_widget(Clear, rect);
    f.render_widget(
        Block::new()
            .borders(Borders::ALL)
            .border_style(Style::new().fg(theme.semantic.warning))
            .title(format!(" {title} ")),
        rect,
    );
    let inner = inner_rect(rect);
    let lines = vec![
        Line::from(Span::styled(body, Style::new().fg(theme.text.primary))),
        Line::default(),
        footer(&[("y", "确认"), ("n/Esc", "取消")], theme),
    ];
    f.render_widget(Paragraph::new(lines), inner);
}

struct InputSpec<'a> {
    title: &'a str,
    prefix: &'a str,
    keys: &'a [(&'a str, &'a str)],
}

fn draw_input(
    f: &mut Frame,
    input: &[char],
    cursor: usize,
    area: Rect,
    theme: &Theme,
    spec: InputSpec<'_>,
    hit_map: &mut HitMapBuilder,
) {
    let rect = centered_rect(76u16.min(area.width.saturating_sub(4)), 7, area);
    if rect.width < 8 || rect.height < 5 {
        return;
    }
    register_modal_root(hit_map, area, rect);
    f.render_widget(Clear, rect);
    f.render_widget(
        Block::new()
            .borders(Borders::ALL)
            .border_style(Style::new().fg(theme.chrome.border_active))
            .title(format!(" {} ", spec.title)),
        rect,
    );
    let inner = inner_rect(rect);
    let cursor = cursor.min(input.len());
    let before: String = input[..cursor].iter().collect();
    let before_width = before.width();
    let at = input
        .get(cursor)
        .map(|ch| ch.to_string())
        .unwrap_or_else(|| " ".into());
    let line = Line::from(vec![
        Span::styled(spec.prefix, Style::new().fg(theme.accent.user)),
        Span::styled(before, Style::new().fg(theme.text.primary)),
        Span::styled(
            at,
            Style::new()
                .fg(theme.text.primary)
                .add_modifier(Modifier::REVERSED),
        ),
    ]);
    let footer_line = footer(spec.keys, theme);
    f.render_widget(
        Paragraph::new(vec![line, Line::default(), footer_line]),
        inner,
    );
    let x = inner
        .x
        .saturating_add(spec.prefix.width() as u16)
        .saturating_add(before_width as u16);
    if x < inner.x.saturating_add(inner.width) {
        f.set_cursor_position((x, inner.y));
    }
}

fn draw_thinking(
    f: &mut Frame,
    scroll: usize,
    body: &str,
    area: Rect,
    theme: &Theme,
    hit_map: &mut HitMapBuilder,
) {
    let rect = centered_rect(
        88u16.min(area.width.saturating_sub(4)),
        area.height.saturating_sub(6),
        area,
    );
    if rect.width < 8 || rect.height < 5 {
        return;
    }
    register_modal_root(hit_map, area, rect);
    f.render_widget(Clear, rect);
    f.render_widget(
        Block::new()
            .borders(Borders::ALL)
            .border_style(Style::new().fg(theme.accent.thinking))
            .title(" 思考回放 · 当前回合 "),
        rect,
    );
    let inner = inner_rect(rect);
    let raw_lines: Vec<&str> = body.lines().collect();
    let start = scroll.min(raw_lines.len().saturating_sub(1));
    let mut lines = Vec::new();
    'outer: for raw in raw_lines.iter().skip(start) {
        for segment in crate::app::render_line::wrap_text(raw, usize::from(inner.width)) {
            if lines.len() >= usize::from(inner.height) {
                break 'outer;
            }
            lines.push(Line::from(Span::styled(
                segment,
                Style::new().fg(theme.text.secondary),
            )));
        }
    }
    if lines.is_empty() {
        lines.push(Line::from(Span::styled(
            "（无内容）",
            Style::new().fg(theme.text.dim),
        )));
    }
    lines.push(footer(
        &[("↑↓/PgUp/PgDn", "滚动"), ("e", "$PAGER"), ("Esc", "关闭")],
        theme,
    ));
    f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);
}

fn inner_rect(rect: Rect) -> Rect {
    Rect {
        x: rect.x.saturating_add(1),
        y: rect.y.saturating_add(1),
        width: rect.width.saturating_sub(2),
        height: rect.height.saturating_sub(2),
    }
}

/// 按**显示宽度**截断（CJK 占两列）。
///
/// 选项必须保证一行放得下：否则 Paragraph 会折行、把后面的选项整体推下去，
/// 鼠标命中的行号就与画面错位了。
fn truncate_width(text: &str, max: usize) -> String {
    if text.width() <= max {
        return text.to_owned();
    }
    if max == 0 {
        return String::new();
    }
    let mut out = String::new();
    let mut used = 0usize;
    for ch in text.chars() {
        let width = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
        if used + width + 1 > max {
            break;
        }
        out.push(ch);
        used += width;
    }
    out.push('…');
    out
}

/// ask 面板的行布局。**渲染与命中测试共用**，两边调同一个函数就不会出现
/// "看着在这行、点着在那行"。行号是 `inner` 内的相对行号（0 起，未计滚动）。
fn ask_rows(
    panel: &AskPanel,
    inner_width: usize,
    theme: &Theme,
    mouse: MouseState,
) -> Vec<ModalRow> {
    let total = panel.questions.len().max(1);
    let focus = panel.focus.min(total.saturating_sub(1));
    let Some(question) = panel.questions.get(focus) else {
        return Vec::new();
    };

    let mut rows: Vec<ModalRow> = Vec::new();
    let mut question_lines = Vec::new();
    push_wrapped(
        &mut question_lines,
        "  ",
        &question.question,
        inner_width,
        Style::new()
            .fg(theme.text.primary)
            .add_modifier(Modifier::BOLD),
    );
    rows.extend(
        question_lines
            .into_iter()
            .map(|line| ModalRow { line, target: None }),
    );
    rows.push(ModalRow {
        line: Line::default(),
        target: None,
    });

    let cursor = panel.option_cursor(focus);
    for (index, option) in question.options.iter().enumerate() {
        let selected = panel.selections[focus] == Some(index);
        let focused = cursor == index;
        let marker = if selected { "◉" } else { "○" };
        let shortcut = option_shortcut_label(index).unwrap_or('·');
        let option_style = if selected {
            Style::new()
                .fg(theme.accent.assistant)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::new().fg(theme.text.primary)
        };
        let target = ModalHit::AskOption {
            question: focus,
            option: index,
        };
        // 前缀固定 7 列：`" ▶ "`(3) + `"{shortcut} "`(2) + `"{marker} "`(2)。
        let text = truncate_width(option, inner_width.saturating_sub(7));
        let spans = vec![
            Span::styled(
                if focused { " ▶ " } else { "   " },
                Style::new().fg(theme.accent.user),
            ),
            Span::styled(format!("{shortcut} "), Style::new().fg(theme.text.dim)),
            Span::styled(format!("{marker} "), option_style),
            Span::styled(text, option_style),
        ];
        rows.push(ModalRow {
            line: button_line(spans, inner_width, mouse.visual(target), theme),
            target: Some(target),
        });
    }

    if question.allow_custom {
        let custom_focused = cursor == question.options.len();
        let custom = panel.customs[focus].trim();
        let marker = if custom.is_empty() { "○" } else { "◉" };
        let text = if panel.editing_custom == Some(focus) {
            format!("自定义> {}_", panel.input)
        } else if custom.is_empty() {
            "自定义输入".to_string()
        } else {
            format!("自定义: {custom}")
        };
        let target = ModalHit::AskCustom { question: focus };
        let spans = vec![
            Span::styled(
                if custom_focused { " ▶ " } else { "   " },
                Style::new().fg(theme.accent.user),
            ),
            Span::styled("z ", Style::new().fg(theme.text.dim)),
            Span::styled(
                format!("{marker} "),
                Style::new().fg(theme.accent.assistant),
            ),
            Span::styled(
                truncate_width(&text, inner_width.saturating_sub(7)),
                Style::new().fg(theme.accent.assistant),
            ),
        ];
        rows.push(ModalRow {
            line: button_line(spans, inner_width, mouse.visual(target), theme),
            target: Some(target),
        });
    }

    if let Some(error) = &panel.error {
        rows.push(ModalRow {
            line: Line::from(Span::styled(
                format!("  ✗ {error}"),
                Style::new().fg(theme.accent.error),
            )),
            target: None,
        });
    }
    rows.push(ModalRow {
        line: Line::default(),
        target: None,
    });
    let hint = if panel.editing_custom.is_some() {
        footer(&[("Enter", "提交并继续"), ("Esc", "取消输入")], theme)
    } else {
        footer(
            &[
                ("←→", "切换问题"),
                ("↑↓", "选择选项"),
                ("Enter/Space", "选择"),
                ("1-9/a-f", "快捷键"),
                ("e/z", "自定义"),
                ("Esc", "跳过"),
            ],
            theme,
        )
    };
    rows.push(ModalRow {
        line: hint,
        target: None,
    });
    rows
}

fn draw_ask(
    f: &mut Frame,
    panel: &AskPanel,
    area: Rect,
    theme: &Theme,
    mouse: MouseState,
    hit_map: &mut HitMapBuilder,
) {
    let total = panel.questions.len().max(1);
    let focus = panel.focus.min(total.saturating_sub(1));

    let rect = ask_rect(area);
    if rect.width < 8 || rect.height < 4 {
        return;
    }
    register_modal_root(hit_map, area, rect);

    f.render_widget(Clear, rect);
    let mode = match panel.mode {
        qaqh_client::AskMode::Single => "single",
        qaqh_client::AskMode::Batch => "batch",
    };
    let block = Block::new()
        .borders(Borders::ALL)
        .border_style(Style::new().fg(theme.chrome.border_active))
        .title(format!(" ❓ 问题 {}/{} · {mode} ", focus + 1, total));
    f.render_widget(block, rect);

    let inner = inner_rect(rect);
    let rows = ask_rows(panel, usize::from(inner.width), theme, mouse);
    // `Paragraph` 按 `panel.scroll` 上滚，所以第 `index` 行画在
    // `inner.y + (index - scroll)`；只有落进视口的那一段才登记。
    let scroll = usize::from(panel.scroll);
    for (index, row) in rows
        .iter()
        .enumerate()
        .skip(scroll)
        .take(usize::from(inner.height))
    {
        let Some(target) = row.target else {
            continue;
        };
        let row_rect = Rect {
            x: inner.x,
            y: inner.y.saturating_add((index - scroll) as u16),
            width: inner.width,
            height: 1,
        };
        register_modal_region(
            hit_map,
            row_rect,
            inner,
            target,
            z::MODAL_CONTENT,
            &row.line,
        );
    }
    f.render_widget(
        Paragraph::new(rows.into_iter().map(|row| row.line).collect::<Vec<_>>())
            .wrap(Wrap { trim: false })
            .scroll((panel.scroll, 0)),
        inner,
    );
}

fn centered_rect(width: u16, height: u16, area: Rect) -> Rect {
    let width = width.min(area.width.saturating_sub(2));
    let height = height.min(area.height.saturating_sub(2));
    Rect {
        x: area.x + area.width.saturating_sub(width) / 2,
        y: area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    }
}

fn push_wrapped(
    out: &mut Vec<Line<'static>>,
    prefix: &str,
    text: &str,
    width: usize,
    style: Style,
) {
    let wrapped = crate::app::render_line::wrap_text(text, width.saturating_sub(prefix.width()));
    for (index, segment) in wrapped.into_iter().enumerate() {
        let prefix = if index == 0 {
            prefix.to_owned()
        } else {
            " ".repeat(prefix.width())
        };
        out.push(Line::from(vec![
            Span::styled(prefix, Style::new()),
            Span::styled(segment, style),
        ]));
    }
}

fn footer(keys: &[(&str, &str)], theme: &Theme) -> Line<'static> {
    let mut spans = Vec::new();
    for (index, (key, description)) in keys.iter().enumerate() {
        if index > 0 {
            spans.push(Span::styled(" · ", Style::new().fg(theme.text.dim)));
        }
        spans.push(Span::styled(
            (*key).to_owned(),
            Style::new()
                .fg(theme.accent.user)
                .add_modifier(Modifier::BOLD),
        ));
        spans.push(Span::styled(
            format!(" {description}"),
            Style::new().fg(theme.text.dim),
        ));
    }
    Line::from(spans)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::session::{PermissionPanel, PlanPanel};
    use crate::theme::{ColorSupport, ThemeKind};
    use crate::ui::v2::hit::{FrameHitMap, FrameId, HitRegion};
    use crate::ui::v2::route::ScreenRoute;
    use qaqh_client::{
        AskMode, DomainAskQuestion as AskQuestion, PermissionCategory, PermissionRisk,
    };
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    /// 造一个空 HitMapBuilder；测试直接调 `draw_*` 时用它吸收登记结果。
    fn scratch_map(route: ModalRoute, width: u16, height: u16) -> HitMapBuilder {
        HitMapBuilder::new(
            FrameId::new(1),
            ScreenRoute::Modal(route),
            ratatui::layout::Size::new(width, height),
            0,
        )
    }

    /// 跑真实 `modal::draw` 并把这一帧的 HitMap 取出来。
    fn draw_modal_to_map(
        app: &App,
        area: Rect,
        route: ModalRoute,
        theme: &Theme,
    ) -> (FrameHitMap, TestBackend) {
        let backend = TestBackend::new(area.width, area.height);
        let mut terminal = Terminal::new(backend).expect("terminal");
        let mut builder = scratch_map(route, area.width, area.height);
        terminal
            .draw(|frame| {
                let drawn = draw(frame, app, frame.area(), theme, route, &mut builder);
                assert!(drawn, "modal 应当渲染出内容");
            })
            .expect("draw modal");
        let map = builder.finish();
        assert!(
            map.validate().is_ok(),
            "绘制阶段登记的 HitMap 必须通过几何校验：{:?}",
            map.validate().err()
        );
        let probe = map.probe(terminal.backend().buffer(), map.frame_id, &map.route);
        assert!(
            probe.is_ok(),
            "真实帧必须通过 strict 探针：{:?}",
            probe.err()
        );
        (map, terminal.backend().clone())
    }

    fn region_for<'a>(map: &'a FrameHitMap, target: &PointerTarget) -> &'a HitRegion {
        map.regions
            .iter()
            .find(|region| &region.target == target)
            .unwrap_or_else(|| panic!("HitMap 必须登记 {target:?}"))
    }

    /// 每个登记区的视觉锚点在真实 buffer 里必须非空——这是 P0-C strict probe 的
    /// 最小预演，锚点选错（落在空格/裁剪区）这里就会红。
    fn assert_anchors_non_empty(backend: &TestBackend, map: &FrameHitMap) {
        let buffer = backend.buffer();
        for region in &map.regions {
            let position = region.anchor.position;
            let cell = &buffer[(position.x, position.y)];
            assert!(
                !cell.symbol().trim().is_empty(),
                "目标 {:?} 的锚点 {:?} 落在空 cell 上",
                region.target,
                position
            );
        }
    }

    fn panel() -> AskPanel {
        AskPanel::new(
            "interaction-1".into(),
            "turn-1".into(),
            AskMode::Batch,
            vec![
                AskQuestion {
                    id: "q1".into(),
                    question: "第一题".into(),
                    options: vec!["A".into(), "B".into()],
                    allow_custom: true,
                },
                AskQuestion {
                    id: "q2".into(),
                    question: "第二题：请选择完整方案".into(),
                    options: vec!["方案一".into(), "方案二".into(), "方案三".into()],
                    allow_custom: true,
                },
            ],
        )
    }

    /// 造一个「带挂起 ask」的 App：`hit_test` 要从 `app.active_session()` 取面板。
    fn app_with_ask(panel: AskPanel) -> App {
        let (mut app, _rx) = App::new_for_test();
        let seed = "seed-ask".to_string();
        let mut session = crate::app::session::SessionState::new(seed.clone());
        session.pending_ask = Some(panel);
        app.tabs.push(seed.clone());
        app.sessions.insert(seed, session);
        app
    }

    fn single_ask() -> AskPanel {
        AskPanel::new(
            "interaction-1".into(),
            "turn-1".into(),
            AskMode::Single,
            vec![AskQuestion {
                id: "q1".into(),
                question: "选哪个？".into(),
                options: vec!["方案甲".into(), "方案乙".into(), "方案丙".into()],
                allow_custom: false,
            }],
        )
    }

    /// 命中区四角可达、外扩一格不可达——这是 spec §9.3/§10 的最小验收面。
    fn assert_region_reachable(map: &FrameHitMap, target: &PointerTarget) {
        let rect = region_for(map, target).rect;
        assert!(!rect.is_empty(), "{target:?} 的矩形不能为空");
        let points = [
            (rect.x, rect.y),
            (rect.right() - 1, rect.y),
            (rect.x, rect.bottom() - 1),
            (rect.right() - 1, rect.bottom() - 1),
            (rect.x + rect.width / 2, rect.y + rect.height / 2),
        ];
        for (x, y) in points {
            let hit = map
                .resolve(x, y, MouseButton::Left)
                .expect("同 z 区域不得重叠");
            assert_eq!(
                hit.map(|region| &region.target),
                Some(target),
                "({x},{y}) 应命中 {target:?}"
            );
        }

        // 外扩一格：横向与纵向各取一次，必须落空或落到别的目标上。
        for (x, y) in [
            (rect.x.saturating_sub(1), rect.y),
            (rect.x, rect.y.saturating_sub(1)),
            (rect.right(), rect.y),
            (rect.x, rect.bottom()),
        ] {
            if x >= map.terminal_size.width
                || y >= map.terminal_size.height
                || crate::ui::v2::hit::contains(rect, x, y)
            {
                continue;
            }
            let hit = map
                .resolve(x, y, MouseButton::Left)
                .expect("同 z 区域不得重叠");
            assert_ne!(
                hit.map(|region| &region.target),
                Some(target),
                "({x},{y}) 在 {target:?} 外扩一格内，不该命中"
            );
        }
    }

    /// 真实 `draw` 生成的 HitMap 必须能把 ask 的选项/自定义行映射成语义目标，
    /// 且四角可达、外扩不可达。
    #[test]
    fn ask_draw_registers_reachable_option_regions() {
        let theme = Theme::resolve(ThemeKind::QaqhNight, ColorSupport::TrueColor);
        let app = app_with_ask(panel());
        let area = Rect::new(0, 0, 100, 30);
        let (map, backend) = draw_modal_to_map(&app, area, ModalRoute::Ask, &theme);
        assert_anchors_non_empty(&backend, &map);

        let option = PointerTarget::Modal(ModalHit::AskOption {
            question: 0,
            option: 1,
        });
        let custom = PointerTarget::Modal(ModalHit::AskCustom { question: 0 });
        assert_region_reachable(&map, &option);
        assert_region_reachable(&map, &custom);

        // 问题正文所在行不是按钮：命中它只能拿到阻断层。
        let question_row = inner_rect(ask_rect(area)).y;
        let hit = map
            .resolve(
                inner_rect(ask_rect(area)).x + 4,
                question_row,
                MouseButton::Left,
            )
            .expect("同 z 区域不得重叠");
        assert_eq!(
            hit.map(|region| &region.target),
            Some(&PointerTarget::ModalRoot),
            "正文不该命中任何按钮"
        );
    }

    /// 滚动后的 ask 只登记**当前视口**里的行，坐标随之整体上移。
    #[test]
    fn ask_draw_follows_scroll_when_registering() {
        let theme = Theme::resolve(ThemeKind::QaqhNight, ColorSupport::TrueColor);
        let mut ask = panel();
        ask.scroll = 2;
        let app = app_with_ask(ask);
        let area = Rect::new(0, 0, 100, 30);
        let (map, _backend) = draw_modal_to_map(&app, area, ModalRoute::Ask, &theme);

        let target = PointerTarget::Modal(ModalHit::AskOption {
            question: 0,
            option: 1,
        });
        let region = region_for(&map, &target);
        // 行序：题目(0) / 空行(1) / 选项 0(2) / 选项 1(3) / 自定义(4)。
        // 滚动 2 行后选项 1 画在视口第 2 行（下标 1）。
        let expected_y = inner_rect(ask_rect(area)).y + 1;
        assert_eq!(region.rect.y, expected_y, "滚动后命中区必须跟着上移");
    }

    fn app_with_permission(panel: PermissionPanel) -> App {
        let (mut app, _rx) = App::new_for_test();
        let seed = "seed-permission".to_string();
        let mut session = crate::app::session::SessionState::new(seed.clone());
        session.pending_permissions.push(panel);
        app.tabs.push(seed.clone());
        app.sessions.insert(seed, session);
        app
    }

    fn app_with_plan(panel: PlanPanel) -> App {
        let (mut app, _rx) = App::new_for_test();
        let seed = "seed-plan".to_string();
        let mut session = crate::app::session::SessionState::new(seed.clone());
        session.pending_plan = Some(panel);
        app.tabs.push(seed.clone());
        app.sessions.insert(seed, session);
        app
    }

    fn permission_panel() -> PermissionPanel {
        PermissionPanel {
            tool_call_id: "tool-1".into(),
            tool_name: "bash".into(),
            action_summary: Some("cargo test --all-targets".into()),
            reason: "运行测试".into(),
            paths: vec!["/tmp/project".into()],
            category: PermissionCategory::Exec,
            level: 2,
            risk: PermissionRisk::High,
            consequence: "会执行本地命令".into(),
            trust_folder: false,
        }
    }

    fn plan_panel() -> PlanPanel {
        PlanPanel {
            interaction_id: "plan-1".into(),
            turn_id: "turn-1".into(),
            plan_content: "第一阶段：完成视觉重构\n第二阶段：补齐测试".into(),
            review_type: "plan".into(),
            todo_items: Vec::new(),
            message: String::new(),
            entering_message: false,
            scroll: 0,
        }
    }

    /// permission：footer 按钮 + 「信任此目录」行都要在真实 draw 里登记。
    #[test]
    fn permission_draw_registers_footer_and_trust_regions() {
        let theme = Theme::resolve(ThemeKind::QaqhNight, ColorSupport::TrueColor);
        let app = app_with_permission(permission_panel());
        let area = Rect::new(0, 0, 100, 30);
        let (map, backend) = draw_modal_to_map(&app, area, ModalRoute::Permission, &theme);
        assert_anchors_non_empty(&backend, &map);

        for hit in [
            ModalHit::PermissionApprove,
            ModalHit::PermissionDeny,
            ModalHit::PermissionTrust,
        ] {
            assert_region_reachable(&map, &PointerTarget::Modal(hit));
        }
    }

    /// plan：只有按钮行可点，正文与滚动提示不可点。
    #[test]
    fn plan_draw_registers_only_action_buttons() {
        let theme = Theme::resolve(ThemeKind::QaqhNight, ColorSupport::TrueColor);
        let app = app_with_plan(plan_panel());
        let area = Rect::new(0, 0, 100, 30);
        let (map, backend) = draw_modal_to_map(&app, area, ModalRoute::Plan, &theme);
        assert_anchors_non_empty(&backend, &map);

        for hit in [
            ModalHit::PlanApprove,
            ModalHit::PlanApproveAutonomous,
            ModalHit::PlanReject,
        ] {
            assert_region_reachable(&map, &PointerTarget::Modal(hit));
        }

        // 正文中心不可点，只能命中阻断层。
        let inner = inner_rect(plan_rect(area));
        let hit = map
            .resolve(inner.x + 2, inner.y, MouseButton::Left)
            .expect("同 z 区域不得重叠");
        assert_eq!(
            hit.map(|region| &region.target),
            Some(&PointerTarget::ModalRoot)
        );
    }

    /// 点在弹窗外命中 `ModalRoot`：Modal 不得穿透到底层（spec §3.4 / §12）。
    #[test]
    fn modal_root_blocks_click_through() {
        let theme = Theme::resolve(ThemeKind::QaqhNight, ColorSupport::TrueColor);
        let app = app_with_ask(single_ask());
        let area = Rect::new(0, 0, 100, 30);
        let (map, _backend) = draw_modal_to_map(&app, area, ModalRoute::Ask, &theme);

        for (x, y) in [(0, 0), (area.width - 1, 0), (0, area.height - 1)] {
            let hit = map
                .resolve(x, y, MouseButton::Left)
                .expect("同 z 区域不得重叠");
            assert_eq!(
                hit.map(|region| &region.target),
                Some(&PointerTarget::ModalRoot),
                "({x},{y}) 在弹窗外必须被阻断层吃掉"
            );
        }
    }

    /// 非左键（右键/中键）不得命中任何 Modal 目标。
    #[test]
    fn modal_regions_only_answer_left_button() {
        let theme = Theme::resolve(ThemeKind::QaqhNight, ColorSupport::TrueColor);
        let app = app_with_permission(permission_panel());
        let area = Rect::new(0, 0, 100, 30);
        let (map, _backend) = draw_modal_to_map(&app, area, ModalRoute::Permission, &theme);
        let rect = region_for(&map, &PointerTarget::Modal(ModalHit::PermissionApprove)).rect;

        for button in [MouseButton::Right, MouseButton::Middle] {
            let hit = map
                .resolve(rect.x + 1, rect.y, button)
                .expect("同 z 区域不得重叠");
            assert_eq!(hit, None, "{button:?} 不该命中 Modal 目标");
        }
    }

    /// spec §5.4：按钮宽度按 **Unicode 显示宽度** 算，不是按 char 个数。
    #[test]
    fn button_row_rects_use_unicode_display_width() {
        let area = Rect::new(0, 0, 40, 1);
        let rects = button_row_rects(&[("批准", ModalHit::PlanApprove)], area);
        // "批准" 显示宽 4（两个 CJK 各占两列），加左右各一格 padding = 6。
        assert_eq!(rects[0].0.width, 6, "CJK 按钮必须按显示宽度算");
    }

    /// spec §5.4：放不下的按钮既不绘制也**不登记**（幽灵 HitRegion 反例）。
    #[test]
    fn narrow_modal_footer_drops_buttons_and_hit_regions() {
        let theme = Theme::resolve(ThemeKind::QaqhNight, ColorSupport::TrueColor);
        let app = app_with_permission(permission_panel());
        // 16 列 → 弹窗宽 12、inner 宽 10：只放得下「a 批准」(8)，放不下「d/Esc 拒绝」(12)。
        let area = Rect::new(0, 0, 16, 12);
        let (map, backend) = draw_modal_to_map(&app, area, ModalRoute::Permission, &theme);
        assert_anchors_non_empty(&backend, &map);

        assert_region_reachable(&map, &PointerTarget::Modal(ModalHit::PermissionApprove));
        assert!(
            !map.regions
                .iter()
                .any(|region| region.target == PointerTarget::Modal(ModalHit::PermissionDeny)),
            "放不下的按钮不能留下可点区域"
        );
    }

    /// 视觉：悬停必须真的把**整行**刷上底色（不是只给文字上色）。
    #[test]
    fn ask_hover_paints_full_row_background() {
        let theme = Theme::resolve(ThemeKind::QaqhNight, ColorSupport::TrueColor);
        let target = ModalHit::AskOption {
            question: 0,
            option: 1,
        };
        let mut app = app_with_ask(single_ask());
        app.modal_hover = Some(target);
        let area = Rect::new(0, 0, 100, 30);

        let backend = TestBackend::new(100, 30);
        let mut terminal = Terminal::new(backend).expect("terminal");
        let mut hit_map = scratch_map(ModalRoute::Ask, 100, 30);
        terminal
            .draw(|frame| {
                let panel = app.active_session().unwrap().pending_ask.as_ref().unwrap();
                draw_ask(
                    frame,
                    panel,
                    frame.area(),
                    &theme,
                    MouseState::from_app(&app),
                    &mut hit_map,
                );
            })
            .expect("draw ask");

        let panel = app.active_session().unwrap().pending_ask.as_ref().unwrap();
        let inner = inner_rect(ask_rect(area));
        let rows = ask_rows(
            panel,
            usize::from(inner.width),
            &theme,
            MouseState::from_app(&app),
        );
        let index = rows
            .iter()
            .position(|row| row.target == Some(target))
            .expect("选项行");
        let row_y = inner.y + index as u16;

        let buffer = terminal.backend().buffer();
        let hovered_cell = &buffer[(inner.x + 2, row_y)];
        assert_eq!(
            hovered_cell.bg, theme.surface.hover,
            "悬停行整行底色应为 surface.hover，实测 {:?}",
            hovered_cell.bg
        );
        // 反例：非悬停行不得被刷上悬停底色。
        let other_y = inner.y + index as u16 + 1;
        assert_ne!(
            buffer[(inner.x + 2, other_y)].bg,
            theme.surface.hover,
            "未悬停的行不该有悬停底色"
        );
    }

    /// 无彩色终端（`NO_COLOR` / `terminal` 主题）下，底色全是 `Reset`：
    /// 必须退回**反显**，否则悬停完全不可见（本机 `NO_COLOR=1` 实测过）。
    #[test]
    fn hover_falls_back_to_reversed_without_colors() {
        let theme = Theme::resolve(ThemeKind::QaqhNight, ColorSupport::NoColor);
        assert_eq!(theme.surface.hover, ratatui::style::Color::Reset);

        let hovered = button_style(ButtonVisual::derive(true, false, true, false), &theme);
        assert!(
            hovered.add_modifier.contains(Modifier::REVERSED),
            "无彩色时悬停必须可见（反显）"
        );
        assert_eq!(hovered.bg, None, "不该硬塞一个不存在的底色");

        let pressed = button_style(ButtonVisual::derive(true, false, true, true), &theme);
        assert!(pressed.add_modifier.contains(Modifier::BOLD));
        assert!(pressed.add_modifier.contains(Modifier::REVERSED));

        // 有彩色的主题仍然走底色，不该被反显顶掉。
        let colorful = Theme::resolve(ThemeKind::QaqhNight, ColorSupport::TrueColor);
        let style = button_style(ButtonVisual::derive(true, false, true, false), &colorful);
        assert_eq!(style.bg, Some(colorful.surface.hover));
        assert!(!style.add_modifier.contains(Modifier::REVERSED));
    }

    /// 按下态要求同时悬停：按下后拖出去 → 视觉立刻回常态（松手也不会提交）。
    #[test]
    fn button_pressed_requires_hover() {
        let target = ModalHit::PlanApprove;
        let other = ModalHit::PlanReject;

        // 在 target 上按下、然后把指针拖到 other 上：target 立刻回常态（不再"按着"）。
        let dragged_out = MouseState {
            hover: Some(other),
            pressed: Some(target),
        };
        let target_visual = dragged_out.visual(target);
        assert_ne!(
            target_visual.state(),
            ButtonState::Pressed,
            "指针离开后不该还是按下态"
        );
        assert_ne!(target_visual.state(), ButtonState::Hovered);

        // other 只是被悬停；按下的不是它，不该显示按下态。
        let other_visual = dragged_out.visual(other);
        assert_eq!(other_visual.state(), ButtonState::Hovered);
        assert_ne!(other_visual.state(), ButtonState::Pressed, "按下的不是它");

        // 指针回到按下时的那个目标上 → 恢复按下态（此时松手才提交）。
        let back = MouseState {
            hover: Some(target),
            pressed: Some(target),
        };
        assert_eq!(back.visual(target).state(), ButtonState::Pressed);
    }

    fn buffer_text(terminal: &Terminal<TestBackend>) -> String {
        terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect()
    }

    #[test]
    fn ask_modal_renders_current_question_with_one_based_shortcuts() {
        let theme = Theme::resolve(ThemeKind::QaqhNight, ColorSupport::TrueColor);
        let mut panel = panel();
        panel.focus = 1;
        panel.selections[1] = Some(1);
        let backend = TestBackend::new(100, 30);
        let mut terminal = Terminal::new(backend).expect("terminal");
        let mut hit_map = scratch_map(ModalRoute::Ask, 100, 30);
        terminal
            .draw(|frame| {
                draw_ask(
                    frame,
                    &panel,
                    frame.area(),
                    &theme,
                    MouseState::default(),
                    &mut hit_map,
                )
            })
            .expect("draw ask");
        let text: String = buffer_text(&terminal)
            .chars()
            .filter(|ch| !ch.is_whitespace())
            .collect();
        assert!(text.contains("问题2/2"), "{text}");
        assert!(text.contains("第二题：请选择完整方案"), "{text}");
        assert!(text.contains("2◉方案二"), "{text}");
        assert!(!text.contains("[0]"), "{text}");
        assert!(!text.contains("第一题"), "{text}");
    }

    #[test]
    fn permission_modal_renders_action_and_risk() {
        let theme = Theme::resolve(ThemeKind::QaqhNight, ColorSupport::TrueColor);
        let panel = PermissionPanel {
            tool_call_id: "tool-1".into(),
            tool_name: "bash".into(),
            action_summary: Some("cargo test --all-targets".into()),
            reason: "运行测试".into(),
            paths: vec!["/tmp/project".into()],
            category: PermissionCategory::Exec,
            level: 2,
            risk: PermissionRisk::High,
            consequence: "会执行本地命令".into(),
            trust_folder: false,
        };
        let backend = TestBackend::new(100, 30);
        let mut terminal = Terminal::new(backend).expect("terminal");
        let mut hit_map = scratch_map(ModalRoute::Permission, 100, 30);
        terminal
            .draw(|frame| {
                draw_permission(
                    frame,
                    &panel,
                    frame.area(),
                    &theme,
                    MouseState::default(),
                    &mut hit_map,
                )
            })
            .expect("draw permission");
        let text: String = buffer_text(&terminal)
            .chars()
            .filter(|ch| !ch.is_whitespace())
            .collect();
        assert!(text.contains("工具权限"));
        assert!(text.contains("cargotest--all-targets"));
        assert!(text.contains("High"));
        assert!(text.contains("批准"));
    }

    #[test]
    fn plan_modal_renders_plan_and_review_actions() {
        let theme = Theme::resolve(ThemeKind::QaqhNight, ColorSupport::TrueColor);
        let panel = PlanPanel {
            interaction_id: "plan-1".into(),
            turn_id: "turn-1".into(),
            plan_content: "第一阶段：完成视觉重构\n第二阶段：补齐测试".into(),
            review_type: "plan".into(),
            todo_items: Vec::new(),
            message: String::new(),
            entering_message: false,
            scroll: 0,
        };
        let backend = TestBackend::new(100, 30);
        let mut terminal = Terminal::new(backend).expect("terminal");
        let mut hit_map = scratch_map(ModalRoute::Plan, 100, 30);
        terminal
            .draw(|frame| {
                draw_plan(
                    frame,
                    &panel,
                    frame.area(),
                    &theme,
                    MouseState::default(),
                    &mut hit_map,
                )
            })
            .expect("draw plan");
        let text: String = buffer_text(&terminal)
            .chars()
            .filter(|ch| !ch.is_whitespace())
            .collect();
        assert!(text.contains("计划评审"));
        assert!(text.contains("第一阶段"));
        assert!(text.contains("批准+自主"));
        assert!(text.contains("拒绝并填写理由"));
    }
}
