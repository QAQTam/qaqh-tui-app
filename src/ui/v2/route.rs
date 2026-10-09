//! V2 屏幕路由。
//!
//! Agent View、Workspace 与 Modal 共用同一张 fullscreen alternate screen。
//! 路由是纯函数，只读取 App 状态，供终端生命周期与绘制共用同一事实源。

use crate::app::{App, Overlay};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkspaceRoute {
    Sessions {
        selected: usize,
        show_archived: bool,
    },
    Settings,
    /// `/remote`：远端直连 + 设备配对页。
    Remote,
    Help,
    History {
        selected: usize,
        detail: bool,
        scroll: usize,
    },
    Todo,
    Subagents {
        selected: usize,
        filter: String,
    },
    Subagent {
        session_id: String,
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
            Overlay::Remote(_) => return ScreenRoute::Workspace(WorkspaceRoute::Remote),
            Overlay::Help => return ScreenRoute::Workspace(WorkspaceRoute::Help),
            Overlay::History {
                selected,
                detail,
                scroll,
            } => {
                return ScreenRoute::Workspace(WorkspaceRoute::History {
                    selected: *selected,
                    detail: *detail,
                    scroll: *scroll,
                });
            }
            Overlay::Subagents { selected, filter } => {
                return ScreenRoute::Workspace(WorkspaceRoute::Subagents {
                    selected: *selected,
                    filter: filter.clone(),
                });
            }
            Overlay::Confirm { .. } => return ScreenRoute::Modal(ModalRoute::Confirm),
            Overlay::AttachPath { .. } => return ScreenRoute::Modal(ModalRoute::AttachPath),
            Overlay::CwdInput { .. } => return ScreenRoute::Modal(ModalRoute::CwdInput),
            Overlay::Thinking { .. } => return ScreenRoute::Modal(ModalRoute::Thinking),
        }
    }

    if let Some(session_id) = app.inspect.clone() {
        return ScreenRoute::Workspace(WorkspaceRoute::Subagent { session_id });
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
        app.tabs.push("session".into());
        app.sessions
            .insert("session".into(), SessionState::new("session".into()));
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
            scroll: 0,
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
            let session = app.sessions.get_mut("session").expect("session");
            session.pending_plan = Some(plan());
            session.pending_ask = Some(ask());
            session.pending_permissions.push(permission());
        }
        assert_eq!(resolve(&app), ScreenRoute::Modal(ModalRoute::Permission));

        app.sessions
            .get_mut("session")
            .expect("session")
            .pending_permissions
            .clear();
        assert_eq!(resolve(&app), ScreenRoute::Modal(ModalRoute::Ask));

        app.sessions
            .get_mut("session")
            .expect("session")
            .pending_ask = None;
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

        // `/history` 也是全屏 Workspace（alternate screen）。
        app.overlays.clear();
        app.overlays.push(Overlay::History {
            selected: 3,
            detail: true,
            scroll: 7,
        });
        assert_eq!(
            resolve(&app),
            ScreenRoute::Workspace(WorkspaceRoute::History {
                selected: 3,
                detail: true,
                scroll: 7,
            })
        );
    }

    #[test]
    fn selector_overlays_route_to_modal() {
        let mut app = app_with_session();
        app.overlays.push(Overlay::Confirm {
            action: crate::app::ConfirmAction::CloseTab("session".into()),
        });
        assert_eq!(resolve(&app), ScreenRoute::Modal(ModalRoute::Confirm));

        app.overlays.clear();
        app.overlays.push(Overlay::Thinking {
            session_id: "session".into(),
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
            ScreenRoute::Workspace(WorkspaceRoute::Subagent {
                session_id: "sub".into()
            })
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
