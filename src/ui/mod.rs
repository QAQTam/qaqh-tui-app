//! UI 根布局：tabbar / 会话信息 / transcript / composer / status + 弹窗层。

pub mod composer;
pub mod modal;
pub mod overlays;
pub mod sidebar;
pub mod status_bar;
pub mod tab_bar;
pub mod theme;
pub mod transcript;

use ratatui::layout::{Constraint, Layout};
use ratatui::Frame;

use crate::app::App;

pub fn draw(f: &mut Frame, app: &App) {
    let area = f.area();
    let [tab_area, main_area, composer_area, status_area] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(3),
        Constraint::Length(composer::height()),
        Constraint::Length(1),
    ])
    .areas(area);

    tab_bar::draw(f, app, tab_area);

    // 三区布局：chat（左）+ workspace 侧栏（右）；composer/status 保持全宽。
    // 主区宽度不足 100 列时自动隐藏侧栏（F4 显式开关优先于 auto，见 show_workspace）。
    let show_ws = app.show_workspace && main_area.width >= 100;
    let (chat_region, ws_area) = if show_ws {
        let [chat, ws] = Layout::horizontal([
            Constraint::Min(0),
            Constraint::Length(sidebar::PREFERRED_WIDTH),
        ])
        .areas(main_area);
        (chat, Some(ws))
    } else {
        (main_area, None)
    };

    let [info_area, transcript_area] =
        Layout::vertical([Constraint::Length(1), Constraint::Min(0)]).areas(chat_region);

    transcript::draw_session_info(f, app, info_area);
    transcript::draw(f, app, transcript_area);
    if let Some(ws) = ws_area {
        sidebar::draw(f, app, ws);
    }

    composer::draw(f, app, composer_area);
    status_bar::draw(f, app, status_area);

    // 会话交互弹窗（active 会话的挂起交互）。
    let has_pending = app
        .active_session()
        .is_some_and(|s| {
            s.active_permission().is_some() || s.pending_ask.is_some() || s.pending_plan.is_some()
        });
    if has_pending {
        modal::draw(f, app, area);
    }

    if !app.overlays.is_empty() {
        overlays::draw(f, app, area);
    }
}
