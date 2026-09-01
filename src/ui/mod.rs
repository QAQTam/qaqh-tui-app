//! UI 根布局：tabbar / 会话信息 / transcript / composer / status + 弹窗层。

pub mod composer;
pub mod home;
pub mod modal;
pub mod overlays;
pub mod settings;
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

    // 首页：无 tab 时居中展示会话列表（视觉参考 opencode Home 弹性留白 + maxW）
    if app.tabs.is_empty() {
        home::draw(f, app, main_area);
    } else {
        // 三区布局：chat（左）+ workspace 侧栏（右）；composer/status 保持全宽。
        // 85 列阈值（原 100 过严，31宽侧栏在 90 列终端亦可共存；F4 显式开关优先）。
        let show_ws = app.show_workspace && main_area.width >= sidebar::MIN_MAIN_WIDTH;
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
    }

    composer::draw(f, app, composer_area);
    // 斜杠一级菜单浮在 composer 上方（不遮 status，不进 overlay 栈）
    composer::draw_slash_menu(f, app, composer_area);
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
