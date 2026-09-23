//! V2 屏幕路由。
//!
//! Agent View 始终使用 inline viewport；Workspace 与 Modal 使用 alternate screen。
//! 路由是纯函数，只读取 App 状态，供终端生命周期与绘制共用同一事实源。

use crate::app::{App, Overlay};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkspaceRoute {
    Sessions {
        selected: usize,
        show_archived: bool,
    },
    Settings,
    Help,
    Todo,
    Subagent {
        seed: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModalRoute {
    Permission,
    Ask,
    Plan,
    Confirm,
    AttachPath,
    CwdInput,
    Thinking,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScreenRoute {
    Agent,
    Workspace(WorkspaceRoute),
    Modal(ModalRoute),
}

/// 解析当前屏幕路由。
///
/// 优先级：挂起交互 Modal > 顶层 overlay > 子代理观测 > todo Workspace。
/// 这保证阻塞式交互不会因后台 overlay/视图状态变化而被静默遮住。
pub fn resolve(app: &App) -> ScreenRoute {
    if let Some(session) = app.active_session() {
        if session.active_permission().is_some() {
            return ScreenRoute::Modal(ModalRoute::Permission);
        }
        if session.pending_ask.is_some() {
            return ScreenRoute::Modal(ModalRoute::Ask);
        }
        if session.pending_plan.is_some() {
            return ScreenRoute::Modal(ModalRoute::Plan);
        }
    }

    if let Some(overlay) = app.overlays.last() {
        match overlay {
            Overlay::SessionList {
                selected,
                show_archived,
            } => {
                return ScreenRoute::Workspace(WorkspaceRoute::Sessions {
                    selected: *selected,
                    show_archived: *show_archived,
                });
            }
            Overlay::Settings(_) => return ScreenRoute::Workspace(WorkspaceRoute::Settings),
            Overlay::Help => return ScreenRoute::Workspace(WorkspaceRoute::Help),
            Overlay::Confirm { .. } => return ScreenRoute::Modal(ModalRoute::Confirm),
            Overlay::AttachPath { .. } => return ScreenRoute::Modal(ModalRoute::AttachPath),
            Overlay::CwdInput { .. } => return ScreenRoute::Modal(ModalRoute::CwdInput),
            Overlay::Thinking { .. } => return ScreenRoute::Modal(ModalRoute::Thinking),
        }
    }

    if let Some(seed) = app.inspect.clone() {
        return ScreenRoute::Workspace(WorkspaceRoute::Subagent { seed });
    }
    if app.show_workspace && app.active_session().is_some() {
        return ScreenRoute::Workspace(WorkspaceRoute::Todo);
    }
    ScreenRoute::Agent
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::session::{AskPanel, PermissionPanel, PlanPanel, SessionState};
    use qaqh_client::{AskMode, PermissionCategory, PermissionRisk};

    fn app_with_session() -> App {
        let (mut app, _rx) = App::new_for_test();
        app.tabs.push("seed".into());
        app.sessions
            .insert("seed".into(), SessionState::new("seed".into()));
        app
    }

    fn ask() -> AskPanel {
        AskPanel::new(
            "ask-1".into(),
            "turn-1".into(),
            AskMode::Single,
            vec![qaqh_client::DomainAskQuestion {
                id: "q1".into(),
                question: "继续？".into(),
                options: vec!["是".into(), "否".into()],
                allow_custom: false,
            }],
        )
    }

    fn permission() -> PermissionPanel {
        PermissionPanel {
            tool_call_id: "tool-1".into(),
            tool_name: "bash".into(),
            action_summary: Some("cargo test".into()),
            reason: String::new(),
            paths: Vec::new(),
            category: PermissionCategory::Exec,
            level: 2,
            risk: PermissionRisk::Medium,
            consequence: String::new(),
            trust_folder: false,
        }
    }

    fn plan() -> PlanPanel {
        PlanPanel {
            interaction_id: "plan-1".into(),
            turn_id: "turn-1".into(),
            plan_content: "计划".into(),
            review_type: "plan".into(),
            todo_items: Vec::new(),
            message: String::new(),
            entering_message: false,
            scroll: 0,
        }
    }

    #[test]
    fn pending_interactions_have_blocking_priority() {
        let mut app = app_with_session();
        app.overlays.push(Overlay::Help);
        {
            let session = app.sessions.get_mut("seed").expect("session");
            session.pending_plan = Some(plan());
            session.pending_ask = Some(ask());
            session.pending_permissions.push(permission());
        }
        assert_eq!(resolve(&app), ScreenRoute::Modal(ModalRoute::Permission));

        app.sessions
            .get_mut("seed")
            .expect("session")
            .pending_permissions
            .clear();
        assert_eq!(resolve(&app), ScreenRoute::Modal(ModalRoute::Ask));

        app.sessions.get_mut("seed").expect("session").pending_ask = None;
        assert_eq!(resolve(&app), ScreenRoute::Modal(ModalRoute::Plan));
    }

    #[test]
    fn workspace_overlays_route_to_alternate_screen() {
        let mut app = app_with_session();
        app.overlays.push(Overlay::SessionList {
            selected: 2,
            show_archived: true,
        });
        assert_eq!(
            resolve(&app),
            ScreenRoute::Workspace(WorkspaceRoute::Sessions {
                selected: 2,
                show_archived: true,
            })
        );

        app.overlays.clear();
        app.overlays.push(Overlay::Settings(Default::default()));
        assert_eq!(
            resolve(&app),
            ScreenRoute::Workspace(WorkspaceRoute::Settings)
        );

        app.overlays.clear();
        app.overlays.push(Overlay::Help);
        assert_eq!(resolve(&app), ScreenRoute::Workspace(WorkspaceRoute::Help));
    }

    #[test]
    fn selector_overlays_route_to_modal() {
        let mut app = app_with_session();
        app.overlays.push(Overlay::Confirm {
            action: crate::app::ConfirmAction::CloseTab("seed".into()),
        });
        assert_eq!(resolve(&app), ScreenRoute::Modal(ModalRoute::Confirm));

        app.overlays.clear();
        app.overlays.push(Overlay::Thinking {
            seed: "seed".into(),
            scroll: 0,
            body: "思考".into(),
        });
        assert_eq!(resolve(&app), ScreenRoute::Modal(ModalRoute::Thinking));
    }

    #[test]
    fn subagent_and_todo_views_are_workspaces() {
        let mut app = app_with_session();
        app.inspect = Some("sub".into());
        assert_eq!(
            resolve(&app),
            ScreenRoute::Workspace(WorkspaceRoute::Subagent { seed: "sub".into() })
        );

        app.inspect = None;
        app.show_workspace = true;
        assert_eq!(resolve(&app), ScreenRoute::Workspace(WorkspaceRoute::Todo));
    }

    #[test]
    fn empty_state_stays_in_agent_view() {
        let (app, _rx) = App::new_for_test();
        assert_eq!(resolve(&app), ScreenRoute::Agent);
    }
}
