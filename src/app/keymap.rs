//! 按键映射纯函数层：把"按键 → 意图"的判定从 `App::handle_key` 的状态副作用中
//! 剥离出来，使其可以脱离 App 状态直接单元测试。
//!
//! 路由顺序纪律（与 `handle_key` 保持一致）：
//! 退出武装 → 全局键 → Alt 切 tab → 交互弹窗（permission > ask > plan）
//! → 覆盖层 → 首页 → composer。

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

/// 全局快捷键意图（与 App 状态解耦的纯映射结果）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum GlobalKey {
    /// Ctrl+C：二次确认退出（首按进入 armed 窗口，由调用方维护窗口状态）。
    QuitArmed,
    /// Ctrl+Q：直接退出。
    QuitNow,
    /// Ctrl+T：新建会话。
    NewSession,
    /// Alt+W：关闭当前标签（弹 Confirm，实际关闭由 Confirm 分支执行）。
    CloseTab,
    /// Ctrl+L：会话列表。
    SessionList,
    /// Ctrl+, / F10：设置面板。
    ToggleSettings,
    /// F1：帮助。
    Help,
    /// F3：思考链显隐。
    ToggleReasoning,
    /// F4：workspace 侧栏。
    ToggleWorkspace,
    /// F6：todo 详情。
    ToggleTodoDetail,
    /// F7：工具卡展开。
    ToggleToolExpand,
}

/// 将按键映射为全局快捷键；非全局键返回 `None`。
///
/// 注意：Ctrl 与 ALT 的判定沿用 `modifiers.contains` 语义（超集匹配），
/// 与重构前 `handle_key` 的行为逐字节一致。
pub(crate) fn map_global_key(key: &KeyEvent) -> Option<GlobalKey> {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    match key.code {
        KeyCode::Char('c') if ctrl => Some(GlobalKey::QuitArmed),
        KeyCode::Char('q') if ctrl => Some(GlobalKey::QuitNow),
        KeyCode::Char('t') if ctrl => Some(GlobalKey::NewSession),
        KeyCode::Char('l') if ctrl => Some(GlobalKey::SessionList),
        KeyCode::Char(',') if ctrl => Some(GlobalKey::ToggleSettings),
        KeyCode::Char('w') if key.modifiers.contains(KeyModifiers::ALT) => {
            Some(GlobalKey::CloseTab)
        }
        KeyCode::F(1) => Some(GlobalKey::Help),
        KeyCode::F(3) => Some(GlobalKey::ToggleReasoning),
        KeyCode::F(4) => Some(GlobalKey::ToggleWorkspace),
        KeyCode::F(6) => Some(GlobalKey::ToggleTodoDetail),
        KeyCode::F(7) => Some(GlobalKey::ToggleToolExpand),
        KeyCode::F(10) => Some(GlobalKey::ToggleSettings),
        _ => None,
    }
}

/// Alt+数字/方向键的目标标签下标；非切 tab 键返回 `None`。
///
/// 越界语义与重构前一致：数字越界时返回原 `active`（按键被消费但不切换）；
/// 空标签列表下方向键返回原 `active`（无操作）。
pub(crate) fn alt_tab_target(active: usize, tab_count: usize, code: KeyCode) -> Option<usize> {
    match code {
        KeyCode::Char(c @ '1'..='9') => {
            let idx = (c as u8 - b'1') as usize;
            Some(if idx < tab_count { idx } else { active })
        }
        KeyCode::Left => Some(if active > 0 {
            active - 1
        } else if tab_count > 0 {
            tab_count - 1
        } else {
            active
        }),
        KeyCode::Right => Some(if active + 1 < tab_count {
            active + 1
        } else {
            0
        }),
        _ => None,
    }
}

/// 挂起交互弹窗的路由（优先级 permission > ask > plan，与 modal 绘制优先级一致）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ModalRoute {
    Permission,
    Ask,
    Plan,
}

/// 由三个挂起态布尔值决定当前弹窗路由；无挂起交互返回 `None`。
pub(crate) fn modal_route(
    has_permission: bool,
    has_ask: bool,
    has_plan: bool,
) -> Option<ModalRoute> {
    if has_permission {
        Some(ModalRoute::Permission)
    } else if has_ask {
        Some(ModalRoute::Ask)
    } else if has_plan {
        Some(ModalRoute::Plan)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode, mods: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, mods)
    }

    #[test]
    fn global_keys_map_to_intents() {
        let ctrl = KeyModifiers::CONTROL;
        assert_eq!(
            map_global_key(&key(KeyCode::Char('c'), ctrl)),
            Some(GlobalKey::QuitArmed)
        );
        assert_eq!(
            map_global_key(&key(KeyCode::Char('q'), ctrl)),
            Some(GlobalKey::QuitNow)
        );
        assert_eq!(
            map_global_key(&key(KeyCode::Char('t'), ctrl)),
            Some(GlobalKey::NewSession)
        );
        assert_eq!(
            map_global_key(&key(KeyCode::Char('l'), ctrl)),
            Some(GlobalKey::SessionList)
        );
        assert_eq!(
            map_global_key(&key(KeyCode::Char(','), ctrl)),
            Some(GlobalKey::ToggleSettings)
        );
        assert_eq!(
            map_global_key(&key(KeyCode::F(10), KeyModifiers::NONE)),
            Some(GlobalKey::ToggleSettings)
        );
    }

    #[test]
    fn alt_w_requires_alt_not_ctrl() {
        // Ctrl+W 不是全局键（避免与终端习惯冲突），Alt+W 才是。
        assert_eq!(
            map_global_key(&key(KeyCode::Char('w'), KeyModifiers::ALT)),
            Some(GlobalKey::CloseTab)
        );
        assert_eq!(
            map_global_key(&key(KeyCode::Char('w'), KeyModifiers::CONTROL)),
            None
        );
    }

    #[test]
    fn plain_and_unknown_keys_are_not_global() {
        assert_eq!(
            map_global_key(&key(KeyCode::Char('t'), KeyModifiers::NONE)),
            None
        );
        assert_eq!(
            map_global_key(&key(KeyCode::F(2), KeyModifiers::NONE)),
            None
        );
        assert_eq!(
            map_global_key(&key(KeyCode::Char('x'), KeyModifiers::CONTROL)),
            None
        );
    }

    #[test]
    fn alt_digit_targets_tab_by_index() {
        assert_eq!(alt_tab_target(0, 3, KeyCode::Char('1')), Some(0));
        assert_eq!(alt_tab_target(0, 3, KeyCode::Char('3')), Some(2));
    }

    #[test]
    fn alt_digit_out_of_range_consumes_without_switching() {
        // 5 个标签按 Alt+7：消费按键但停在原位（重构前行为）。
        assert_eq!(alt_tab_target(2, 5, KeyCode::Char('7')), Some(2));
        assert_eq!(alt_tab_target(0, 0, KeyCode::Char('1')), Some(0));
    }

    #[test]
    fn alt_arrows_wrap_around() {
        assert_eq!(alt_tab_target(2, 3, KeyCode::Left), Some(1));
        // 最左再向左 → 环绕到最右。
        assert_eq!(alt_tab_target(0, 3, KeyCode::Left), Some(2));
        // 空列表：无操作。
        assert_eq!(alt_tab_target(0, 0, KeyCode::Left), Some(0));
        assert_eq!(alt_tab_target(1, 3, KeyCode::Right), Some(2));
        // 最右向右 → 环绕到最左。
        assert_eq!(alt_tab_target(2, 3, KeyCode::Right), Some(0));
    }

    #[test]
    fn non_tab_keys_return_none() {
        assert_eq!(alt_tab_target(0, 3, KeyCode::Char('x')), None);
        assert_eq!(alt_tab_target(0, 3, KeyCode::Up), None);
        // '0' 不在 1..=9 映射内。
        assert_eq!(alt_tab_target(0, 3, KeyCode::Char('0')), None);
    }

    #[test]
    fn modal_route_priority_permission_ask_plan() {
        assert_eq!(modal_route(true, true, true), Some(ModalRoute::Permission));
        assert_eq!(modal_route(false, true, true), Some(ModalRoute::Ask));
        assert_eq!(modal_route(false, false, true), Some(ModalRoute::Plan));
        assert_eq!(modal_route(false, false, false), None);
    }
}
