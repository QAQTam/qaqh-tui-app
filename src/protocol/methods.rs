#![allow(dead_code)]
//! 服务面方法表（镜像 `qaqh-runtime/src/ringing/service_methods.rs`）。
//!
//! 单一权威清单：`POST /ringing/v1/service/{method}`。会话生命周期方法
//! （session.new/session.resume 等）**刻意不在表中**（N5），只走命令面。
//! Read = 无副作用查询（错误码 `query_failed`）；Write = 变更（`action_failed`）。
//! 未列出的名字 → 404 `unknown_method`。

use super::command::ConversationMode;

pub const DAEMON_VERSION: &str = "daemon.version";
pub const SESSION_LIST: &str = "session.list";
pub const SESSION_META: &str = "session.meta";
pub const SESSION_ACTIVITY: &str = "session.activity";
pub const SESSION_DASHBOARD: &str = "session.dashboard";
pub const SESSION_GET_ACTIVITY: &str = "session.get_activity";
pub const WORKSPACE_GET: &str = "workspace.get";
pub const WORKSPACE_STATUS: &str = "workspace.status";
pub const WORKSPACE_LIST: &str = "workspace.list";
pub const WORKSPACE_DIAGNOSE: &str = "workspace.diagnose";
pub const WORKSPACE_SET: &str = "workspace.set";
pub const WORKSPACE_SET_MODE: &str = "workspace.set_mode";
pub const WORKSPACE_INSTALL_WSL: &str = "workspace.install_wsl";
pub const WORKSPACE_CREATE: &str = "workspace.create";
pub const WORKSPACE_RENAME: &str = "workspace.rename";
pub const WORKSPACE_DELETE: &str = "workspace.delete";
pub const WORKSPACE_MOVE_SESSION: &str = "workspace.move_session";
pub const WORKSPACE_DETACH: &str = "workspace.detach";
pub const FS_LIST: &str = "fs.list";
pub const FS_READ: &str = "fs.read";
pub const CONFIG_LOAD: &str = "config.load";
pub const CONFIG_SAVE: &str = "config.save";
pub const CONFIG_SET_PERMISSION_LEVEL: &str = "config.set_permission_level";
pub const PROFILE_APPLY: &str = "profile.apply";
pub const PROFILE_SAVE_CURRENT: &str = "profile.save_current";
pub const PROFILE_DELETE: &str = "profile.delete";
pub const SKILLS_LIST_TOOLS: &str = "skills.list_tools";
pub const SKILLS_OPERATION: &str = "skills.operation";
pub const SKILLS_RELOAD: &str = "skills.reload";
pub const TODO_STATUS: &str = "todo.status";
pub const PLAN_READ: &str = "plan.read";
pub const PLAN_CONTEXT_STATS: &str = "plan.context_stats";
pub const STATS_TOKEN_USAGE: &str = "stats.token_usage";
pub const GIT_DIFF: &str = "git.diff";
pub const GIT_BRANCH: &str = "git.branch";
pub const GIT_BRANCHES: &str = "git.branches";
pub const GIT_FILE_DIFF: &str = "git.file_diff";
pub const GIT_SWITCH_BRANCH: &str = "git.switch_branch";
pub const GIT_COMMIT: &str = "git.commit";
pub const SUBAGENT_SPAWN: &str = "subagent.spawn";
pub const SESSION_SET_TOOL_MODE: &str = "session.set_tool_mode";

/// 方法类别（决定错误码形状）。
#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MethodKind {
    Read,
    Write,
}

#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MethodInfo {
    pub kind: MethodKind,
    /// 要求 params 携带 seed 并做 lease 归属校验。
    pub requires_seed: bool,
}

const READ: MethodInfo = MethodInfo { kind: MethodKind::Read, requires_seed: false };
const READ_SEEDED: MethodInfo = MethodInfo { kind: MethodKind::Read, requires_seed: true };
const WRITE: MethodInfo = MethodInfo { kind: MethodKind::Write, requires_seed: false };

/// 与后端 `lookup` 完全一致的方法表（22 Read + 19 Write）。
pub fn lookup(method: &str) -> Option<MethodInfo> {
    match method {
        DAEMON_VERSION => Some(READ),
        SESSION_LIST => Some(READ),
        SESSION_META => Some(READ_SEEDED),
        SESSION_ACTIVITY => Some(READ),
        SESSION_DASHBOARD => Some(READ_SEEDED),
        SESSION_GET_ACTIVITY => Some(READ_SEEDED),
        WORKSPACE_GET => Some(READ_SEEDED),
        WORKSPACE_STATUS => Some(READ),
        WORKSPACE_LIST => Some(READ),
        WORKSPACE_DIAGNOSE => Some(READ),
        WORKSPACE_SET => Some(WRITE),
        WORKSPACE_SET_MODE => Some(WRITE),
        WORKSPACE_INSTALL_WSL => Some(WRITE),
        WORKSPACE_CREATE => Some(WRITE),
        WORKSPACE_RENAME => Some(WRITE),
        WORKSPACE_DELETE => Some(WRITE),
        WORKSPACE_MOVE_SESSION => Some(WRITE),
        WORKSPACE_DETACH => Some(WRITE),
        FS_LIST => Some(READ),
        FS_READ => Some(READ),
        CONFIG_LOAD => Some(READ),
        CONFIG_SAVE => Some(WRITE),
        CONFIG_SET_PERMISSION_LEVEL => Some(WRITE),
        PROFILE_APPLY => Some(WRITE),
        PROFILE_SAVE_CURRENT => Some(WRITE),
        PROFILE_DELETE => Some(WRITE),
        SKILLS_LIST_TOOLS => Some(READ),
        SKILLS_OPERATION => Some(WRITE),
        SKILLS_RELOAD => Some(WRITE),
        TODO_STATUS => Some(READ_SEEDED),
        PLAN_READ => Some(READ_SEEDED),
        PLAN_CONTEXT_STATS => Some(READ_SEEDED),
        STATS_TOKEN_USAGE => Some(READ),
        GIT_DIFF => Some(READ_SEEDED),
        GIT_BRANCH => Some(READ_SEEDED),
        GIT_BRANCHES => Some(READ_SEEDED),
        GIT_FILE_DIFF => Some(READ_SEEDED),
        GIT_SWITCH_BRANCH => Some(WRITE),
        GIT_COMMIT => Some(WRITE),
        SUBAGENT_SPAWN => Some(WRITE),
        SESSION_SET_TOOL_MODE => Some(WRITE),
        _ => None,
    }
}

/// 会话列表条目（daemon 返回 SessionMeta + 运行时字段；宽松解析）。
#[derive(Debug, Clone, Default)]
pub struct SessionMetaView {
    pub seed: String,
    pub created_at: Option<u64>,
    pub updated_at: Option<u64>,
    pub model: Option<String>,
    pub turn_count: Option<u64>,
    pub message_count: Option<u64>,
    pub last_summary: Option<String>,
    pub title: Option<String>,
    pub cwd: Option<String>,
    pub tool_mode: Option<String>,
    /// 0=Code, 1=Plan（meta 编码）。
    pub mode: Option<u8>,
    pub archived: bool,
    pub ephemeral: bool,
    pub running: bool,
}

impl SessionMetaView {
    pub fn parse(value: &serde_json::Value) -> Option<Self> {
        let obj = value.as_object()?;
        Some(Self {
            seed: obj.get("seed")?.as_str()?.to_owned(),
            created_at: obj.get("created_at").and_then(serde_json::Value::as_u64),
            updated_at: obj.get("updated_at").and_then(serde_json::Value::as_u64),
            model: obj.get("model").and_then(serde_json::Value::as_str).map(str::to_owned),
            turn_count: obj.get("turn_count").and_then(serde_json::Value::as_u64),
            message_count: obj.get("message_count").and_then(serde_json::Value::as_u64),
            last_summary: obj
                .get("last_summary")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned),
            title: obj.get("title").and_then(serde_json::Value::as_str).map(str::to_owned),
            cwd: obj.get("cwd").and_then(serde_json::Value::as_str).map(str::to_owned),
            tool_mode: obj
                .get("tool_mode")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned),
            mode: obj.get("mode").and_then(serde_json::Value::as_u64).map(|v| v as u8),
            archived: obj.get("archived").and_then(serde_json::Value::as_bool).unwrap_or(false),
            ephemeral: obj.get("ephemeral").and_then(serde_json::Value::as_bool).unwrap_or(false),
            running: obj.get("running").and_then(serde_json::Value::as_bool).unwrap_or(false),
        })
    }

    /// 展示标题：title → last_summary → cwd 尾段 → seed。
    pub fn display_title(&self) -> String {
        if let Some(t) = self.title.as_deref().filter(|s| !s.is_empty()) {
            return t.to_owned();
        }
        if let Some(s) = self.last_summary.as_deref().filter(|s| !s.is_empty()) {
            return s.to_owned();
        }
        if let Some(c) = self.cwd.as_deref().filter(|s| !s.is_empty()) {
            let trimmed = c.trim_end_matches(['/', '\\']);
            if let Some(idx) = trimmed.rfind(['/', '\\']) {
                return trimmed[idx + 1..].to_owned();
            }
            return trimmed.to_owned();
        }
        self.seed.clone()
    }

    pub fn conversation_mode(&self) -> ConversationMode {
        match self.mode {
            Some(1) => ConversationMode::Plan,
            _ => ConversationMode::Code,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn method_table_matches_backend_shape() {
        assert_eq!(lookup("session.list"), Some(READ));
        assert_eq!(lookup("session.meta"), Some(READ_SEEDED));
        assert_eq!(lookup("session.new"), None, "生命周期方法刻意不在表中");
        assert_eq!(lookup("session.resume"), None);
        assert_eq!(lookup("session/list"), None, "旧 slash 别名已拆除");
        assert_eq!(lookup("config.save"), Some(WRITE));
        assert_eq!(lookup("bogus"), None);
    }

    #[test]
    fn session_meta_display_title_fallbacks() {
        let v = serde_json::json!({
            "seed": "0123abcd",
            "cwd": "F:\\code\\qaqh",
            "last_summary": "修复 SSE 解码",
            "archived": true,
            "running": true,
            "mode": 1
        });
        let meta = SessionMetaView::parse(&v).unwrap();
        assert_eq!(meta.display_title(), "修复 SSE 解码");
        assert!(meta.archived && meta.running);
        assert_eq!(meta.conversation_mode(), ConversationMode::Plan);
    }
}
